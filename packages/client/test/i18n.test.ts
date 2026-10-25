import fs from 'node:fs';
import ts from 'typescript';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ago, span, when } from '../src/format.js';
import { failure, speak, speaking, t } from '../src/i18n.js';

afterEach(() => {
  vi.unstubAllGlobals();
  speak('en');
});

describe('language selection', () => {
  it('normalizes regional tags and follows the system when cleared', () => {
    vi.stubGlobal('navigator', { language: 'ko-KR' });
    speak('en-US');
    expect(t('Files')).toBe('Files');
    speak('');
    expect(speaking()).toBe('ko');
    expect(t('Files')).toBe('파일');
    speak('KO_kr');
    expect(speaking()).toBe('ko');
    speak('fr-FR');
    expect(speaking()).toBe('en');
  });

  it('interpolates account values after translation without translating user data', () => {
    speak('ko');
    expect(t('Hello, {email}', { email: 'me@example.com' })).toBe('me@example.com님, 반가워요');
  });
});

describe('localized history', () => {
  const today = new Date(2026, 9, 25, 14, 30).getTime();

  it('uses Korean dates, elapsed time and durations together', () => {
    speak('ko');
    expect(ago(today, today)).toBe('방금');
    expect(ago(today - 120_000, today)).toBe('2분 전');
    expect(ago(today - 7_200_000, today)).toBe('2시간 전');
    expect(ago(today - 86_400_000, today)).toBe('1일 전');
    expect(when(today, today)).toBe('오늘 · 14:30');
    expect(when(today - 86_400_000, today)).toBe('어제 · 14:30');
    expect(when(new Date(2026, 8, 5, 18, 22).getTime(), today)).toBe('9월 5일 · 18:22');
    expect(span(18_000)).toBe('18초');
    expect(span(2_760_000)).toBe('46분');
    expect(span(7_560_000)).toBe('2시간 6분');
  });

  it('retains English grammar and compact durations', () => {
    speak('en');
    expect(ago(today - 60_000, today)).toBe('1 minute ago');
    expect(ago(today - 120_000, today)).toBe('2 minutes ago');
    expect(when(today, today)).toBe('Today · 14:30');
    expect(span(7_560_000)).toBe('2h 06m');
  });
});

describe('localized failures', () => {
  it('translates expected account failures and explains transport failures', () => {
    speak('ko');
    expect(failure('The email, password or code is wrong.')).toBe('이메일, 비밀번호 또는 인증 코드가 올바르지 않아요.');
    expect(failure('the account server could not be reached: connection refused')).toContain('네트워크 연결을 확인');
    expect(failure('the account server sent an answer this build could not read: invalid JSON')).toContain('서버 응답을 읽지 못했어요');
    expect(failure('unexpected native failure')).toBe('요청을 처리하지 못했어요. 다시 시도해 주세요.');
    speak('en');
    expect(failure('unexpected native failure')).toBe('unexpected native failure');
  });
});

it('covers every literal translation and dynamic label table in the client', () => {
  speak('ko');
  const missing = new Set<string>();
  const directory = new URL('../src/', import.meta.url);
  for (const file of fs.readdirSync(directory).filter((name) => /\.tsx?$/.test(name) && name !== 'i18n.ts')) {
    const source = ts.createSourceFile(file, fs.readFileSync(new URL(file, directory), 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
    /** Checks string leaves in translation selectors without treating interpolation data as copy. */
    const check = (node: ts.Node): void => {
      if (ts.isStringLiteral(node) && t(node.text) === node.text) missing.add(node.text);
      if (ts.isConditionalExpression(node)) { check(node.whenTrue); check(node.whenFalse); }
    };
    /** Traverses application calls and the shared tables translated at their point of use. */
    const visit = (node: ts.Node): void => {
      if (ts.isCallExpression(node) && node.expression.getText(source) === 't' && node.arguments[0]) check(node.arguments[0]);
      if (ts.isVariableDeclaration(node) && ['KINDS', 'FIELD_NAMES', 'GRANTS', 'SAID'].includes(node.name.getText(source)) && node.initializer) {
        /** Collects names and descriptions while leaving IDs, styles and fields alone. */
        const labels = (child: ts.Node): void => {
          if (ts.isPropertyAssignment(child) && ['name', 'about', 'why', 'watching', 'shared', 'off'].includes(child.name.getText(source))) check(child.initializer);
          if (node.name.getText(source) === 'FIELD_NAMES' && ts.isPropertyAssignment(child)) check(child.initializer);
          ts.forEachChild(child, labels);
        };
        labels(node.initializer);
      }
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
  expect([...missing]).toEqual([]);
});
