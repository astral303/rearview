/// A provider's declaration for one operation that differs between agents.
/// The app runs the operation through `T`; a marker capability holds `()`.
pub enum Capability<T = ()> {
    Capable(T),
    NotCapable,
}

impl<T> Capability<T> {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "tests read it; app code does not")
    )]
    pub fn is_capable(&self) -> bool {
        matches!(self, Self::Capable(_))
    }
}
