/// As `GLib`'s `g_format_size` does by default, so the sizes match what GNOME shows.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return if bytes == 1 {
            "1 byte".to_owned()
        } else {
            format!("{bytes} bytes")
        };
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = UNITS[0];
    for next in &UNITS[1..] {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::format_size;

    #[test]
    fn matches_glib_style() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1), "1 byte");
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(1000), "1.0 kB");
        assert_eq!(format_size(1_500_000), "1.5 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.1 GB");
        assert_eq!(format_size(10_000_000_000_000), "10.0 TB");
        assert_eq!(format_size(u64::MAX), "18.4 EB");
    }
}
