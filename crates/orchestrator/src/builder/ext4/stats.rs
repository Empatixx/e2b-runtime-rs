pub fn parse_free_blocks(output: &str) -> Result<i64, String> {
    let groups = output.lines().filter(|line| is_group_header(line)).count();
    let counters: Vec<&str> = output.lines().filter_map(group_free_blocks).collect();
    if groups == 0 || counters.len() != groups {
        return Err(format!(
            "could not parse free blocks for every block group: found {groups} groups and {} counters",
            counters.len()
        ));
    }
    counters.iter().try_fold(0i64, |total, counter| {
        counter
            .parse::<i64>()
            .map(|blocks| total + blocks)
            .map_err(|e| format!("could not parse block-group free blocks: {e}"))
    })
}

pub fn parse_reserved_blocks(output: &str) -> Result<i64, String> {
    const LABEL: &str = "Reserved block count:";
    let after = output
        .find(LABEL)
        .map(|index| &output[index + LABEL.len()..])
        .filter(|after| after.starts_with(char::is_whitespace))
        .map(|after| after.trim_start())
        .map(|after| &after[..after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len())])
        .filter(|digits| !digits.is_empty())
        .ok_or("could not find reserved blocks in debugfs output")?;
    after
        .parse()
        .map_err(|e| format!("could not parse reserved blocks: {e}"))
}

fn is_group_header(line: &str) -> bool {
    line.trim_start_matches([' ', '\t'])
        .strip_prefix("Group")
        .filter(|rest| rest.starts_with([' ', '\t']))
        .map(|rest| rest.trim_start_matches([' ', '\t']))
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(number, _)| !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit()))
}

fn group_free_blocks(line: &str) -> Option<&str> {
    let rest = line.strip_prefix([' ', '\t'])?.trim_start_matches([' ', '\t']);
    let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
    let (digits, after) = rest.split_at(digits_end);
    let after = after.strip_prefix([' ', '\t'])?.trim_start_matches([' ', '\t']);
    let after = after.strip_prefix("free block")?;
    let after = after.strip_prefix('s').unwrap_or(after);
    (!digits.is_empty() && after.starts_with(',')).then_some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_blocks_match_upstream_vectors() {
        let two = "Free blocks:              50000\n Group  0: block bitmap at 1, inode bitmap at 2, inode table at 3\n           28629 free blocks, 100 free inodes, 1 used directory\n Group  1: block bitmap at 4, inode bitmap at 5, inode table at 6\n           28639 free blocks, 100 free inodes, 1 used directory\n";
        let single = " Group  0: block bitmap at 1, inode bitmap at 2, inode table at 3\n           1 free block, 1 free inode, 1 used directory\n";
        let missing = " Group  0: block bitmap at 1, inode bitmap at 2, inode table at 3\n           10 free blocks, 100 free inodes, 1 used directory\n Group  1: block bitmap at 4, inode bitmap at 5, inode table at 6\n";

        assert_eq!(parse_free_blocks(two), Ok(57268));
        assert_eq!(parse_free_blocks(single), Ok(1));
        assert_eq!(
            parse_free_blocks(missing),
            Err("could not parse free blocks for every block group: found 2 groups and 1 counters".to_owned())
        );
        assert_eq!(
            parse_free_blocks("Free blocks:              50000\n"),
            Err("could not parse free blocks for every block group: found 0 groups and 0 counters".to_owned())
        );
    }

    #[test]
    fn reserved_blocks_match_upstream_vectors() {
        assert_eq!(
            parse_reserved_blocks(
                "Block count:              131072\nReserved block count:     6553\nFree blocks:              120000\n"
            ),
            Ok(6553)
        );
        assert_eq!(parse_reserved_blocks("Reserved block count:     0\n"), Ok(0));
        assert_eq!(
            parse_reserved_blocks("Block count: 1\n"),
            Err("could not find reserved blocks in debugfs output".to_owned())
        );
    }
}
