pub const MAX_CPUS: usize = 8192;

pub fn parse_mask(mask: &str) -> Result<Vec<usize>, String> {
    if mask.is_empty() {
        return Ok(Vec::new());
    }
    mask.split(',')
        .try_fold((Vec::new(), 0), |(mut cpus, next), part| {
            let (start, end) = parse_range(part).map_err(|e| range_error(part, &e))?;
            if start < next || end < start {
                return Err(range_error(part, "out of order"));
            }
            cpus.extend(start..=end);
            Ok((cpus, end + 1))
        })
        .map(|(cpus, _)| cpus)
}

pub fn parse_cpu(text: &str) -> Result<usize, String> {
    Some(text)
        .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|text| text.parse::<u16>().ok())
        .map(usize::from)
        .filter(|cpu| *cpu < MAX_CPUS)
        .ok_or_else(|| format!("not a CPU number below {MAX_CPUS}"))
}

fn parse_range(part: &str) -> Result<(usize, usize), String> {
    let (low, high) = part
        .split_once('-')
        .map_or((part, None), |(low, high)| (low, Some(high)));
    let start = parse_cpu(low)?;
    let end = high.map_or(Ok(start), parse_cpu)?;
    Ok((start, end))
}

fn range_error(part: &str, reason: &str) -> String {
    let shown: String = part.chars().take(16).collect();
    format!("cpu list range {shown:?}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_kernel_cpulist_format() {
        for (input, expected) in [
            ("", vec![]),
            ("0", vec![0]),
            ("0-3", vec![0, 1, 2, 3]),
            ("0-1,4,6-7", vec![0, 1, 4, 6, 7]),
        ] {
            assert_eq!(parse_mask(input), Ok(expected), "{input}");
        }
    }

    #[test]
    fn rejects_everything_the_kernel_does_not_print() {
        let long = "9".repeat(1 << 16);
        for input in [
            "0-x",
            "0,,2",
            "0,",
            ",0",
            "-",
            "-1",
            "+1",
            "0x1",
            "1e3",
            "0 ,1",
            "1-2-3",
            "\0",
            "\u{ff11}",
            "3-1",
            "1,0",
            "0-3,2",
            "0-3,3",
            "8192",
            "0-8192",
            "99999999999999999999",
            long.as_str(),
        ] {
            assert!(parse_mask(input).is_err(), "{:.32}", input);
        }
    }

    #[test]
    fn the_largest_mask_holds_every_cpu() {
        let cpus = parse_mask("0-8191").unwrap();
        assert_eq!(cpus.len(), MAX_CPUS);
        assert!(cpus.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn errors_name_the_range_and_not_the_whole_input() {
        let error = parse_mask(&format!("{}x", "9".repeat(100))).unwrap_err();
        assert!(error.starts_with("cpu list range \"9999999999999999\": "), "{error}");
    }
}
