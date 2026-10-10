#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    New,
    Replace,
    Refuse,
}

/// The service holds both processes open, so equal IDs are the same process.
pub(crate) fn start(owner: Option<u32>, asking: u32) -> Start {
    match owner {
        None => Start::New,
        Some(owner) if owner == asking => Start::Replace,
        Some(_) => Start::Refuse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_that_owns_the_helper_replaces_it_and_another_app_is_refused() {
        assert_eq!(start(None, 42), Start::New);
        assert_eq!(start(Some(42), 42), Start::Replace);
        assert_eq!(start(Some(42), 43), Start::Refuse);
    }
}
