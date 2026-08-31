//! WGS84 latitude/longitude to UTM, for sampling USGS elevation COGs.
//!
//! The 3DEP 1 m tiles are published on a UTM grid (one zone per 6° of
//! longitude), so placing a texel in one means projecting its lat/lon the same
//! way the data was. This is the Krüger series to third order — good to well
//! under a millimetre inside a zone, far past what a terrain heightmap can
//! represent.
//!
//! The tiles carry NAD83 while the import works in WGS84. The two ellipsoids
//! are identical for this purpose (GRS80 and WGS84 differ in flattening by
//! ~1e-10), and the datum realisations differ by a metre or two across the
//! continental US — under two texels, and a uniform shift rather than a
//! distortion, so it is not corrected for.

/// Semi-major axis of the WGS84 ellipsoid, metres.
const A: f64 = 6_378_137.0;
/// Flattening.
const F: f64 = 1.0 / 298.257_223_563;
/// UTM's scale factor along a zone's central meridian.
const K0: f64 = 0.9996;
/// Easting of every zone's central meridian, metres.
const FALSE_EASTING: f64 = 500_000.0;
/// Northing offset applied in the southern hemisphere, metres.
const FALSE_NORTHING: f64 = 10_000_000.0;

/// A point on the UTM grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Utm {
    /// Longitude zone, 1..=60.
    pub zone: u8,
    pub easting: f64,
    pub northing: f64,
}

/// The UTM zone containing a longitude.
pub(crate) fn zone_for(longitude: f64) -> u8 {
    (((longitude + 180.0) / 6.0).floor() as i32).clamp(0, 59) as u8 + 1
}

/// Project WGS84 degrees onto the UTM grid.
///
/// `zone` forces a zone, so a map straddling a zone boundary can be sampled
/// in one frame rather than tearing; `None` picks the one the longitude falls
/// in. Accuracy degrades gradually outside a zone's own 6°, which is why the
/// forced case is fine for the couple of degrees a map can span.
pub(crate) fn project(latitude: f64, longitude: f64, zone: Option<u8>) -> Utm {
    let zone = zone.unwrap_or_else(|| zone_for(longitude));
    let central_meridian = ((zone as f64 - 1.0) * 6.0 - 180.0 + 3.0).to_radians();

    let n = F / (2.0 - F);
    let n2 = n * n;
    let n3 = n2 * n;
    let a_bar = A / (1.0 + n) * (1.0 + n2 / 4.0 + n2 * n2 / 64.0);

    let lat = latitude.to_radians();
    let delta_lon = longitude.to_radians() - central_meridian;
    let sin_lat = lat.sin();

    // Conformal latitude, expressed through its tangent.
    let two_root_n = 2.0 * n.sqrt() / (1.0 + n);
    let t = (sin_lat.atanh() - two_root_n * (two_root_n * sin_lat).atanh()).sinh();
    let xi = (t / delta_lon.cos()).atan();
    let eta = (delta_lon.sin() / (1.0 + t * t).sqrt()).atanh();

    let alpha = [
        n / 2.0 - 2.0 / 3.0 * n2 + 5.0 / 16.0 * n3,
        13.0 / 48.0 * n2 - 3.0 / 5.0 * n3,
        61.0 / 240.0 * n3,
    ];
    let mut e = eta;
    let mut north = xi;
    for (j, a) in alpha.iter().enumerate() {
        let k = 2.0 * (j + 1) as f64;
        e += a * (k * xi).cos() * (k * eta).sinh();
        north += a * (k * xi).sin() * (k * eta).cosh();
    }

    Utm {
        zone,
        easting: K0 * a_bar * e + FALSE_EASTING,
        northing: K0 * a_bar * north + if latitude < 0.0 { FALSE_NORTHING } else { 0.0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each point must land inside the 1 m tile that actually covers it.
    /// The extents come from the GeoTIFF tiepoints of the products the
    /// National Map returns for those exact coordinates, so this pins the
    /// zone, datum and false easting against published data — to 10 km, which
    /// is enough to catch any gross error, with the exact anchors below.
    #[test]
    fn lands_inside_the_covering_usgs_tile() {
        // (lat, lon, zone, easting range, northing range)
        let cases = [
            // USGS_1M_11_x51y402_CA_FEMAR9Southeast_D24
            (
                36.2400,
                -116.8200,
                11u8,
                (509_994.0, 520_006.0),
                (4_009_994.0, 4_020_006.0),
            ),
            // USGS_1M_12_x40y400_AZ_GrandCanyonNP_2019_B19
            (
                36.0980,
                -112.0970,
                12,
                (399_994.0, 410_006.0),
                (3_989_994.0, 4_000_006.0),
            ),
            // USGS one meter x32y413 UT ZionNP QL1 2016
            (
                37.2982,
                -113.0263,
                12,
                (319_994.0, 330_006.0),
                (4_119_994.0, 4_130_006.0),
            ),
        ];
        for (lat, lon, zone, east, north) in cases {
            let utm = project(lat, lon, None);
            assert_eq!(utm.zone, zone, "zone at {lat},{lon}");
            assert!(
                utm.easting >= east.0 && utm.easting <= east.1,
                "easting {} outside {east:?} at {lat},{lon}",
                utm.easting
            );
            assert!(
                utm.northing >= north.0 && utm.northing <= north.1,
                "northing {} outside {north:?} at {lat},{lon}",
                utm.northing
            );
        }
    }

    #[test]
    fn central_meridian_sits_at_the_false_easting() {
        // Zone 12 runs 114°W..108°W, so its central meridian is 111°W.
        let utm = project(40.0, -111.0, None);
        assert_eq!(utm.zone, 12);
        assert!(
            (utm.easting - FALSE_EASTING).abs() < 1e-6,
            "easting {} should be exactly the false easting",
            utm.easting
        );
    }

    #[test]
    fn northing_tracks_ground_distance() {
        // A degree of latitude is ~110.9 km here; UTM shrinks it by k0 on the
        // central meridian.
        let a = project(36.0, -111.0, None);
        let b = project(37.0, -111.0, None);
        let degree = b.northing - a.northing;
        assert!(
            (degree - 110_900.0 * K0).abs() < 400.0,
            "a degree of latitude measured {degree} m"
        );
    }

    #[test]
    fn forcing_a_zone_keeps_neighbours_in_one_frame() {
        // 114°W is the zone 11/12 boundary. Sampled either side in zone 12,
        // two points 1 km apart must stay 1 km apart.
        let west = project(40.0, -114.005, Some(12));
        let east = project(40.0, -113.995, Some(12));
        assert_eq!((west.zone, east.zone), (12, 12));
        let span = east.easting - west.easting;
        let expected = 0.01 * 111_320.0 * 40.0f64.to_radians().cos();
        assert!(
            (span - expected).abs() / expected < 0.01,
            "span {span} m vs expected {expected} m"
        );
        // Without forcing, the west point lands in zone 11 and its easting
        // jumps by hundreds of kilometres.
        assert_eq!(project(40.0, -114.005, None).zone, 11);
    }

    #[test]
    fn zones_cover_the_whole_range() {
        assert_eq!(zone_for(-180.0), 1);
        assert_eq!(zone_for(-177.0), 1);
        assert_eq!(zone_for(-174.1), 1);
        assert_eq!(zone_for(-174.0), 2);
        assert_eq!(zone_for(0.0), 31);
        assert_eq!(zone_for(179.9), 60);
        assert_eq!(zone_for(180.0), 60);
    }
}
