use gettextrs::{bind_textdomain_codeset, bindtextdomain};
use tracing::{debug, warn};

pub fn try_init() {
    let mo_dir = concat!(env!("OUT_DIR"), "/mo/");
    if !std::path::Path::new(mo_dir).exists() {
        eprintln!(
            "{}",
            crate::l10n::fmt(
                crate::l10n::t_("l10n_debug: mo_dir does not exist: {mo_dir}"),
                &[("mo_dir", &mo_dir.to_string())]
            )
        );
    }
    let _ = bindtextdomain("systema", mo_dir);
    if let Err(e) = bindtextdomain("systema", mo_dir) {
        eprintln!(
            "{}",
            crate::l10n::fmt(
                crate::l10n::t_("l10n_debug: Failed to bindtextdomain: {e}"),
                &[("e", &e.to_string())]
            )
        );
    }
    let _ = bind_textdomain_codeset("systema", "UTF-8");
    if let Err(e) = bind_textdomain_codeset("systema", "UTF-8") {
        eprintln!(
            "{}",
            crate::l10n::fmt(
                crate::l10n::t_("l10n_debug: Failed to bind_textdomain_codeset: {e}"),
                &[("e", &e.to_string())]
            )
        );
    }
    eprintln!(
        "{}",
        crate::l10n::fmt(
            crate::l10n::t_("l10n_debug: Initialized gettext with mo_dir: {mo_dir}"),
            &[("mo_dir", &mo_dir.to_string())]
        )
    );
}
