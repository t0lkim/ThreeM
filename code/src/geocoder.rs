use reverse_geocoder::{ReverseGeocoder, SearchResult};
use tracing::debug;

/// Wrapper around the reverse geocoder with a pre-loaded k-d tree
pub struct GeoLookup {
    geocoder: ReverseGeocoder,
}

impl GeoLookup {
    /// Create a new geocoder (loads the GeoNames dataset into a k-d tree)
    pub fn new() -> Self {
        debug!("loading GeoNames dataset for reverse geocoding");
        let geocoder = ReverseGeocoder::new();
        Self { geocoder }
    }

    /// Look up the nearest city/country for a GPS coordinate
    pub fn lookup(&self, lat: f64, lon: f64) -> Option<LocationInfo> {
        let result: SearchResult = self.geocoder.search((lat, lon));

        let record = result.record;
        let name = record.name.to_string();
        let country = record.cc.to_string();

        let location = sanitise_for_filename(&format!("{}-{}", name, country));

        debug!(lat, lon, location = %location, "reverse geocoded");

        Some(LocationInfo {
            city: name,
            country,
            filename_part: location,
        })
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LocationInfo {
    pub city: String,
    pub country: String,
    pub filename_part: String,
}

fn sanitise_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else if c == ' ' {
                '-'
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitise_for_filename() {
        assert_eq!(sanitise_for_filename("New York-US"), "New-York-US");
        assert_eq!(sanitise_for_filename("São Paulo/BR"), "São-Paulo_BR");
    }

    #[test]
    fn test_lookup_known_location() {
        let geo = GeoLookup::new();
        let result = geo.lookup(51.5074, -0.1278);
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.country, "GB");
    }

    #[test]
    fn test_lookup_returns_filename_part() {
        let geo = GeoLookup::new();
        let result = geo.lookup(40.7128, -74.0060).unwrap();
        assert!(!result.filename_part.is_empty());
        assert!(!result.filename_part.contains(' '));
        assert!(!result.filename_part.contains('/'));
    }
}
