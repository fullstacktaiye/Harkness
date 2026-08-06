#[cxx_qt::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        /// The small Rust-backed object exposed to the Harkness QML module.
        #[qobject]
        #[qml_element]
        #[qproperty(QString, greeting, READ, CONSTANT)]
        type HarknessBackend = super::HarknessBackendRust;
    }
}

pub struct HarknessBackendRust {
    greeting: cxx_qt_lib::QString,
}

impl Default for HarknessBackendRust {
    fn default() -> Self {
        Self {
            greeting: harkness_core::greeting().into(),
        }
    }
}
