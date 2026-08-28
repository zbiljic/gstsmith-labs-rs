use memchr::memmem;

pub(crate) fn find_start_code(input: &[u8], from: usize) -> Option<(usize, usize)> {
    let suffix_start = memmem::find(input.get(from..)?, &[0, 0, 1])?;
    let suffix_start = from.checked_add(suffix_start)?;
    if suffix_start > from && input.get(suffix_start.checked_sub(1)?) == Some(&0) {
        Some((suffix_start.checked_sub(1)?, 4))
    } else {
        Some((suffix_start, 3))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Case = (&'static [u8], usize, Option<(usize, usize)>);

    fn scalar_find_start_code(input: &[u8], from: usize) -> Option<(usize, usize)> {
        let mut offset = from;
        while offset.checked_add(3)? <= input.len() {
            if input.get(offset) == Some(&0) && input.get(offset + 1) == Some(&0) {
                if input.get(offset + 2) == Some(&1) {
                    return Some((offset, 3));
                }
                if offset.checked_add(4)? <= input.len()
                    && input.get(offset + 2) == Some(&0)
                    && input.get(offset + 3) == Some(&1)
                {
                    return Some((offset, 4));
                }
            }
            offset = offset.checked_add(1)?;
        }
        None
    }

    #[test]
    fn handles_annex_b_edge_cases() {
        let cases: &[Case] = &[
            (&[], 0, None),
            (&[0], 0, None),
            (&[0, 0], 0, None),
            (&[0, 0, 1], 0, Some((0, 3))),
            (&[0, 0, 0, 1], 0, Some((0, 4))),
            (&[0, 0, 0, 0, 1], 0, Some((1, 4))),
            (&[0, 0, 0, 0, 1], 1, Some((1, 4))),
            (&[0, 0, 0, 0, 1], 2, Some((2, 3))),
            (&[0, 0, 0, 0, 1], 3, None),
            (&[0, 0, 0, 0, 0, 1], 0, Some((2, 4))),
            (&[0x55, 0, 0, 1], 0, Some((1, 3))),
            (&[0x55, 0, 0, 0, 1], 0, Some((1, 4))),
            (&[0x55, 0x66, 0, 0, 1], 2, Some((2, 3))),
            (&[0, 0, 1, 0xaa, 0, 0, 0, 1], 0, Some((0, 3))),
            (&[0, 0, 1, 0xaa, 0, 0, 0, 1], 3, Some((4, 4))),
            (&[0, 0, 1, 0, 0, 1], 0, Some((0, 3))),
            (&[0, 0, 1, 0, 0, 1], 3, Some((3, 3))),
            (&[0, 0, 1, 0, 0], 0, Some((0, 3))),
            (&[0, 0, 0], 0, None),
            (&[0, 0, 0, 0, 0], 0, None),
            (&[0, 0, 1], 1, None),
            (&[0, 0, 1], 2, None),
        ];

        for &(input, from, expected) in cases {
            assert_eq!(
                find_start_code(input, from),
                expected,
                "input={input:?}, from={from}"
            );
            assert_eq!(
                find_start_code(input, from),
                scalar_find_start_code(input, from),
                "scalar mismatch for input={input:?}, from={from}"
            );
        }
    }

    #[test]
    fn matches_scalar_reference_for_deterministic_inputs() {
        let mut state = 0x9e37_79b9_u32;
        for length in 0..=12 {
            for _case in 0..256 {
                let mut input = vec![0_u8; length];
                for byte in &mut input {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *byte = (state >> 24) as u8;
                }
                for from in 0..=length.saturating_add(1) {
                    assert_eq!(
                        find_start_code(&input, from),
                        scalar_find_start_code(&input, from),
                        "scalar mismatch for input={input:?}, from={from}"
                    );
                }
            }
        }
    }

    #[test]
    fn finds_successive_and_empty_nal_boundaries() {
        let input = [0, 0, 1, 0xaa, 0, 0, 1, 0, 0, 0, 1, 0xbb];
        let first = find_start_code(&input, 0).expect("first prefix");
        assert_eq!(first, (0, 3));
        let second = find_start_code(&input, first.0 + first.1).expect("second prefix");
        assert_eq!(second, (4, 3));
        let third = find_start_code(&input, second.0 + second.1).expect("third prefix");
        assert_eq!(third, (7, 4));
        assert_eq!(third.0, second.0 + second.1);
    }
}
