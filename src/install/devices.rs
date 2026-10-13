//! Pixel device pool for `--device` validation.
//!
//! The AIO installer covers every Tensor Pixel; the codename pool is the
//! union of all families from `device/google/pixels/devices/` (one dir per
//! device, family from its `pixel.json`). An unknown codename fails the
//! install immediately instead of flashing a touch section for the wrong
//! board. Regenerate this table when a device directory is added there.

/// (codename, SoC family), sorted by family then codename.
pub const PIXEL_DEVICES: &[(&str, &str)] = &[
    ("bluejay", "gs101"),
    ("oriole", "gs101"),
    ("raven", "gs101"),
    ("cheetah", "gs201"),
    ("felix", "gs201"),
    ("lynx", "gs201"),
    ("panther", "gs201"),
    ("tangorpro", "gs201"),
    ("akita", "zuma"),
    ("husky", "zuma"),
    ("shiba", "zuma"),
    ("caiman", "zumapro"),
    ("comet", "zumapro"),
    ("komodo", "zumapro"),
    ("stallion", "zumapro"),
    ("tegu", "zumapro"),
    ("tokay", "zumapro"),
    ("blazer", "laguna"),
    ("frankel", "laguna"),
    ("mustang", "laguna"),
    ("rango", "laguna"),
    ("cubs", "malibu"),
    ("grizzly", "malibu"),
    ("kodiak", "malibu"),
    ("yogi", "malibu"),
];

/// SoC family for a codename, or None when outside the pool.
pub fn family_of(codename: &str) -> Option<&'static str> {
    PIXEL_DEVICES
        .iter()
        .find(|(c, _)| *c == codename)
        .map(|(_, f)| *f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_covers_all_families() {
        let mut fams: Vec<&str> = PIXEL_DEVICES.iter().map(|(_, f)| *f).collect();
        fams.sort();
        fams.dedup();
        assert_eq!(fams, vec!["gs101", "gs201", "laguna", "malibu", "zuma", "zumapro"]);
        assert_eq!(PIXEL_DEVICES.len(), 25);
    }

    #[test]
    fn lookup_spot_checks() {
        assert_eq!(family_of("shiba"), Some("zuma"));
        assert_eq!(family_of("raven"), Some("gs101"));
        assert_eq!(family_of("yogi"), Some("malibu"));
        assert_eq!(family_of("mustang"), Some("laguna"));
        assert_eq!(family_of("notapixel"), None);
        assert_eq!(family_of(""), None);
    }
}
