//! TupleBase core: database logic with no UI dependencies.

/// Product name shown by every front end.
pub const PRODUCT_NAME: &str = "TupleBase";

/// Greeting rendered by the hello-world window.
pub fn greeting() -> String {
    format!("Hello from {PRODUCT_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_names_the_product() {
        assert_eq!(greeting(), "Hello from TupleBase");
    }
}
