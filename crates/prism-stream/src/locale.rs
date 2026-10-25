//! The language shared by native controls in each Prism process.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether native controls use Korean, read without allocating in the window loop.
static KOREAN: AtomicBool = AtomicBool::new(false);

/// The system language, resolved once through the platform locale API.
static SYSTEM: OnceLock<String> = OnceLock::new();

/// Applies an explicit preference, or follows the system when the preference is empty.
pub fn set_language(preference: &str) {
    let system = SYSTEM.get_or_init(|| sys_locale::get_locale().unwrap_or_else(|| "en".to_owned()));
    KOREAN.store(resolve(preference, system), Ordering::Relaxed);
}

/// Chooses native copy in the selected language.
#[must_use]
pub fn text<'a>(english: &'a str, korean: &'a str) -> &'a str {
    if KOREAN.load(Ordering::Relaxed) {
        korean
    } else {
        english
    }
}

/// Resolves regional Korean tags and falls back to English for unsupported languages.
fn resolve(preference: &str, system: &str) -> bool {
    let tag = if preference.is_empty() {
        system
    } else {
        preference
    };
    tag.split(['-', '_'])
        .next()
        .is_some_and(|language| language.eq_ignore_ascii_case("ko"))
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn an_explicit_choice_overrides_the_system_and_regional_tags_resolve() {
        assert!(resolve("", "ko-KR"));
        assert!(resolve("KO_kr", "en-US"));
        assert!(!resolve("en", "ko-KR"));
        assert!(!resolve("fr-FR", "en-US"));
        assert!(!resolve("", ""));
    }
}
