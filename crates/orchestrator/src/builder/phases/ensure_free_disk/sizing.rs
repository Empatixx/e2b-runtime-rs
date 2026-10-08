use crate::builder::ext4::BLOCK_SIZE;

const MB: i64 = 1 << 20;
const GROWTH_ALLOWANCE_DIVISOR: i64 = 10;

pub fn compute_grown_size(current_size: i64, target_free: i64, free_before: i64) -> Result<i64, String> {
    if current_size <= 0 {
        return Err(format!("invalid current size: {current_size}"));
    }
    if target_free < 0 {
        return Err(format!("invalid target free space: {target_free}"));
    }
    if free_before >= target_free {
        return Err(format!(
            "free space before ({free_before}) is already at or above target ({target_free})"
        ));
    }
    let shortage = target_free
        .checked_sub(free_before)
        .ok_or("free-space shortage overflows")?;
    let growth = shortage
        .checked_add(allowance(shortage))
        .ok_or("free-space growth overflows")?;
    let size = current_size.checked_add(growth).ok_or("grown size overflows")?;
    align_up(size).ok_or_else(|| "aligned grown size overflows".to_owned())
}

fn allowance(shortage: i64) -> i64 {
    shortage / GROWTH_ALLOWANCE_DIVISOR + i64::from(shortage % GROWTH_ALLOWANCE_DIVISOR != 0)
}

fn align_up(size: i64) -> Option<i64> {
    match size % MB {
        0 => Some(size),
        remainder => size.checked_add(MB - remainder),
    }
}

pub fn validate_source_geometry(
    source_size: i64,
    block_size: i64,
    header_size: u64,
    header_block_size: u64,
) -> Result<(), String> {
    if source_size <= 0 {
        return Err(format!("invalid source size: {source_size}"));
    }
    if block_size != BLOCK_SIZE {
        return Err(format!("unsupported source block size: {block_size}"));
    }
    let (header_size_signed, header_block_size_signed) =
        match (i64::try_from(header_size), i64::try_from(header_block_size)) {
            (Ok(size), Ok(block)) => (size, block),
            _ => return Err("source header geometry overflows".to_owned()),
        };
    if source_size != header_size_signed || block_size != header_block_size_signed {
        return Err(format!(
            "source geometry mismatch: device={source_size}/{block_size} header={header_size}/{header_block_size}"
        ));
    }
    match source_size % block_size {
        0 => Ok(()),
        _ => Err(format!("source size {source_size} is not block aligned")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: i64 = 1 << 30;

    #[test]
    fn the_grown_size_follows_upstream_vectors() {
        assert_eq!(compute_grown_size(CURRENT, 512 * MB, 512 * MB - 1), Ok(CURRENT + MB));
        assert_eq!(compute_grown_size(CURRENT, 512 * MB, -MB), Ok(CURRENT + 565 * MB));
        assert_eq!(compute_grown_size(CURRENT, 15666 * MB, 0), Ok(CURRENT + 17233 * MB));
    }

    #[test]
    fn the_grown_size_overflows_are_named() {
        assert_eq!(
            compute_grown_size(CURRENT, i64::MAX, 0),
            Err("free-space growth overflows".to_owned())
        );
        assert_eq!(
            compute_grown_size(i64::MAX, 1, 0),
            Err("grown size overflows".to_owned())
        );
        assert_eq!(
            compute_grown_size(CURRENT, i64::MAX, -1),
            Err("free-space shortage overflows".to_owned())
        );
        assert_eq!(
            compute_grown_size(i64::MAX - 2 * MB, MB, 0),
            Err("aligned grown size overflows".to_owned())
        );
    }

    #[test]
    fn the_grown_size_refuses_nonsense_inputs() {
        assert_eq!(compute_grown_size(0, MB, 0), Err("invalid current size: 0".to_owned()));
        assert_eq!(
            compute_grown_size(CURRENT, -1, -5),
            Err("invalid target free space: -1".to_owned())
        );
        assert_eq!(
            compute_grown_size(CURRENT, MB, MB),
            Err(format!("free space before ({MB}) is already at or above target ({MB})"))
        );
    }

    #[test]
    fn the_grown_size_is_a_whole_number_of_mebibytes() {
        let size = compute_grown_size(CURRENT, 100 * MB + 7, 3).unwrap();

        assert_eq!(size % MB, 0);
        assert!(size > CURRENT + 100 * MB);
    }

    #[test]
    fn the_source_geometry_is_validated_like_upstream() {
        let size = 16 << 20;
        assert_eq!(validate_source_geometry(size, 4096, size as u64, 4096), Ok(()));
        assert_eq!(
            validate_source_geometry(size + 4096, 4096, size as u64, 4096),
            Err(format!(
                "source geometry mismatch: device={}/4096 header={size}/4096",
                size + 4096
            ))
        );
        assert_eq!(
            validate_source_geometry(size, 8192, size as u64, 8192),
            Err("unsupported source block size: 8192".to_owned())
        );
        assert_eq!(
            validate_source_geometry(0, 4096, 0, 4096),
            Err("invalid source size: 0".to_owned())
        );
        assert_eq!(
            validate_source_geometry(size, 4096, u64::MAX, 4096),
            Err("source header geometry overflows".to_owned())
        );
        assert_eq!(
            validate_source_geometry(size + 1, 4096, size as u64 + 1, 4096),
            Err(format!("source size {} is not block aligned", size + 1))
        );
    }
}
