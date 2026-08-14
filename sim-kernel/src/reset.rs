//! Device reset (QEMU `Resettable` hold phase).

/// Hardware reset. Construction (`new`) must not apply this; call after wiring.
pub trait Resettable {
    fn reset(&mut self);
}
