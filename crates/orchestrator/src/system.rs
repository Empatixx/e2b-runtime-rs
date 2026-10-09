pub fn all_ok(results: impl IntoIterator<Item = Result<(), String>>) -> Result<(), String> {
    let errors: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    errors.is_empty().then_some(()).ok_or_else(|| errors.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ok_joins_every_error() {
        assert_eq!(all_ok([Ok(()), Ok(())]), Ok(()));
        assert_eq!(
            all_ok([Err("a".to_owned()), Ok(()), Err("b".to_owned())]),
            Err("a; b".to_owned())
        );
    }
}
