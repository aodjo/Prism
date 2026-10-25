/** Keeps each window in the saved language and applies changes from the other windows. */

import { useEffect, useState } from 'react';

import type { Settings } from './api.js';
import { speak } from './i18n.js';

/**
 * Loads the language preference and subscribes before reading to avoid overwriting a newer choice.
 *
 * @returns {string} The saved preference, including an empty string for the system language.
 */
export function useLanguage(): string {
  const [preference, setPreference] = useState('');

  useEffect(() => {
    let active = true;
    let changed = false;
    /** Applies language changes while this window remains mounted. */
    const apply = (settings: Settings): void => {
      if (active) {
        speak(settings.language);
        setPreference(settings.language);
      }
    };
    const stop = window.prism.onSettings((settings) => {
      changed = true;
      apply(settings);
    });
    void window.prism.getSettings().then((settings) => {
      if (!changed) apply(settings);
    }).catch(console.error);

    return () => {
      active = false;
      stop();
    };
  }, []);

  return preference;
}
