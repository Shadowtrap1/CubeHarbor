//! Java version selection.

/// Java versions we ship server images for.
pub const AVAILABLE: [u8; 4] = [8, 17, 21, 25];

/// Smallest shipped Java that satisfies `required`.
pub fn image_java(required: u8) -> u8 {
    AVAILABLE.iter().copied().find(|v| *v >= required).unwrap_or(*AVAILABLE.last().unwrap())
}

/// Offline fallback for when Mojang's metadata is unreachable or the version is unknown there.
pub fn guess_for_minecraft(version: &str) -> u8 {
    let parts: Vec<u32> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    match parts.as_slice() {
        // Year-based versions (26.1+) need Java 25.
        [major, ..] if *major >= 26 => 25,
        [1, minor, rest @ ..] => {
            let patch = rest.first().copied().unwrap_or(0);
            match minor {
                0..=16 => 8,
                17 => 17,
                18 | 19 => 17,
                20 if patch < 5 => 17,
                _ => 21,
            }
        }
        _ => 21,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses() {
        assert_eq!(guess_for_minecraft("1.12.2"), 8);
        assert_eq!(guess_for_minecraft("1.16.5"), 8);
        assert_eq!(guess_for_minecraft("1.18.2"), 17);
        assert_eq!(guess_for_minecraft("1.20.1"), 17);
        assert_eq!(guess_for_minecraft("1.20.6"), 21);
        assert_eq!(guess_for_minecraft("1.21.1"), 21);
        assert_eq!(guess_for_minecraft("26.3"), 25);
    }

    #[test]
    fn images() {
        assert_eq!(image_java(16), 17);
        assert_eq!(image_java(21), 21);
        assert_eq!(image_java(30), 25);
    }
}
