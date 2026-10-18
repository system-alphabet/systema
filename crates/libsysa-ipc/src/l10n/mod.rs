mod l10n_gettext;

#[cfg(all(debug_assertions, feature = "l10n_debug"))]
mod l10n_debug;

pub fn t_(msgid: &str) -> String {
    l10n_gettext::t_(msgid)
}

pub fn n_(msgid: &str, msgid_plural: &str, n: u32) -> String {
    l10n_gettext::n_(msgid, msgid_plural, n)
}

pub fn fmt(template: impl AsRef<str>, args: &[(&str, &str)]) -> String {
    l10n_gettext::fmt(template, args)
}

pub fn init() {
    l10n_gettext::init();
    #[cfg(all(debug_assertions, feature = "l10n_debug"))]
    l10n_debug::try_init();
}
