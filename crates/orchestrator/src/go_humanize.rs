const SIZES: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
const BASE: f64 = 1000.0;

pub fn bytes(size: u64) -> String {
    if size < 10 {
        return format!("{size} B");
    }
    let exponent = ((size as f64).ln() / BASE.ln()).floor();
    let value = ((size as f64) / BASE.powf(exponent) * 10.0 + 0.5).floor() / 10.0;
    let suffix = SIZES[exponent as usize];
    match value < 10.0 {
        true => format!("{value:.1} {suffix}"),
        false => format!("{value:.0} {suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_go_humanize() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(9), "9 B");
        assert_eq!(bytes(1000), "1.0 kB");
        assert_eq!(bytes(2_500_000), "2.5 MB");
        assert_eq!(bytes(26_214_400_000), "26 GB");
        assert_eq!(bytes(82_854_982), "83 MB");
    }
}
