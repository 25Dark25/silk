//! English and Spanish texts. The language follows the Windows display
//! language (Spanish for any Spanish locale, English for everything else)
//! and can be forced with `SILK_LANG=en` or `SILK_LANG=es`.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
}

static LANG: OnceLock<Lang> = OnceLock::new();

pub fn lang() -> Lang {
    *LANG.get_or_init(detect)
}

fn detect() -> Lang {
    if let Ok(forced) = std::env::var("SILK_LANG") {
        return parse(&forced);
    }
    if system_is_spanish() { Lang::Es } else { Lang::En }
}

fn parse(code: &str) -> Lang {
    if code.trim().to_ascii_lowercase().starts_with("es") { Lang::Es } else { Lang::En }
}

#[cfg(windows)]
fn system_is_spanish() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    const LANG_SPANISH: u16 = 0x0A;
    // The low 10 bits of a LANGID are the primary language, whatever the
    // country (Spain, Mexico, Argentina…).
    (unsafe { GetUserDefaultUILanguage() } & 0x3FF) == LANG_SPANISH
}

#[cfg(not(windows))]
fn system_is_spanish() -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_some_and(|v| parse(&v) == Lang::Es)
}

/// The text for the current language.
pub fn tr(en: &'static str, es: &'static str) -> &'static str {
    match lang() {
        Lang::En => en,
        Lang::Es => es,
    }
}

/// `format!` with one template per language: `trf!("Done in {t}", "Listo en {t}")`.
macro_rules! trf {
    ($en:literal, $es:literal $(, $arg:expr)* $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => format!($en $(, $arg)*),
            $crate::i18n::Lang::Es => format!($es $(, $arg)*),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_language_codes() {
        for code in ["es", "ES", "es-ES", "es_MX.UTF-8", " es "] {
            assert_eq!(parse(code), Lang::Es, "{code}");
        }
        for code in ["en", "en-US", "fr-FR", "", "C"] {
            assert_eq!(parse(code), Lang::En, "{code}");
        }
    }
}
