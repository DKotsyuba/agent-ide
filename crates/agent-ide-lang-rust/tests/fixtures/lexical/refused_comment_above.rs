pub struct Raw(*mut u8);

// SAFETY: the pointer is never shared.
unsafe impl Send for Raw {}
