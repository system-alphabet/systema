use gettextrs::{
    bind_textdomain_codeset, bindtextdomain, gettext, ngettext, textdomain, LocaleCategory,
};

pub fn t_(msgid: &str) -> String {
    gettext(msgid)
}

pub fn n_(msgid: &str, msgid_plural: &str, n: u32) -> String {
    ngettext(msgid, msgid_plural, n)
}

pub fn fmt(template: impl AsRef<str>, args: &[(&str, &str)]) -> String {
    let mut result = template.as_ref().to_string();
    for (key, val) in args {
        result = result.replace(&format!("{{{}}}", key), val);
    }
    result
}

pub fn init() {
    let _ = gettextrs::setlocale(LocaleCategory::LcAll, "");
    let _ = bindtextdomain("systema", crate::paths::instance().locale_dir);
    let _ = textdomain("systema");
    let _ = bind_textdomain_codeset("systema", "UTF-8");
}
