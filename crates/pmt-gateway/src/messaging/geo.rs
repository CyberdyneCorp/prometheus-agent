//! Locations as structured attachments (spec: platform-adapters, `WhatsApp`
//! interactive and rich features). An inbound location (or live location)
//! is an [`Attachment`](super::adapter::Attachment) of kind
//! [`MediaKind::Location`](super::descriptor::MediaKind::Location) or
//! `LiveLocation` whose `location` is a `geo:<lat>,<lon>` URI (RFC 5870)
//! and whose `name` is the place's name or address; the model reads it as
//! one line of text. Outbound, a reply line `LOCATION: <lat>,<lon> [name]`
//! becomes a location pin on platforms that send them (an outbound
//! attachment with the `geo:` URI as its path, the name after `#`), and a
//! map link everywhere else.

use super::adapter::OutboundAttachment;

/// A point on the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    /// Latitude, -90..=90.
    pub latitude: f64,
    /// Longitude, -180..=180.
    pub longitude: f64,
    /// The place's name or address.
    pub name: Option<String>,
}

impl Location {
    /// A location when the coordinates are on the map.
    #[must_use]
    pub fn new(latitude: f64, longitude: f64, name: Option<String>) -> Option<Self> {
        let valid = (-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude);
        valid.then(|| Self {
            latitude,
            longitude,
            name: name.filter(|n| !n.trim().is_empty()),
        })
    }

    /// `geo:<lat>,<lon>`.
    #[must_use]
    pub fn uri(&self) -> String {
        format!("geo:{},{}", self.latitude, self.longitude)
    }

    /// Reads a `geo:` URI (parameters after `;` ignored; the name after
    /// `#`, as outbound attachments carry it).
    #[must_use]
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("geo:")?;
        let (point, name) = match rest.split_once('#') {
            Some((point, name)) => (point, Some(name.to_owned())),
            None => (rest, None),
        };
        let point = point.split(';').next()?;
        let (lat, lon) = point.split_once(',')?;
        Self::new(lat.trim().parse().ok()?, lon.trim().parse().ok()?, name)
    }

    /// The line the model reads.
    #[must_use]
    pub fn describe(&self, live: bool) -> String {
        let what = if live { "Live location" } else { "Location" };
        match &self.name {
            Some(name) => format!("[{what}: {name} ({}, {})]", self.latitude, self.longitude),
            None => format!("[{what}: {}, {}]", self.latitude, self.longitude),
        }
    }

    /// A map link, for platforms without location pins.
    #[must_use]
    pub fn link(&self) -> String {
        let url = format!(
            "https://www.openstreetmap.org/?mlat={0}&mlon={1}#map=16/{0}/{1}",
            self.latitude, self.longitude
        );
        match &self.name {
            Some(name) => format!("📍 {name}: {url}"),
            None => format!("📍 {url}"),
        }
    }

    /// The outbound attachment sending it as a pin.
    #[must_use]
    pub fn attachment(&self) -> OutboundAttachment {
        let path = match &self.name {
            Some(name) => format!("{}#{name}", self.uri()),
            None => self.uri(),
        };
        OutboundAttachment::new(&path)
    }
}

/// The location a reply line asks to send (`LOCATION: <lat>,<lon> [name]`).
#[must_use]
pub fn directive(line: &str) -> Option<Location> {
    let rest = line.trim().strip_prefix("LOCATION:")?.trim();
    let (point, name) = match rest.split_once(char::is_whitespace) {
        Some((point, name)) => (point, Some(name.trim().to_owned())),
        None => (rest, None),
    };
    let (lat, lon) = point.split_once(',')?;
    Location::new(lat.parse().ok()?, lon.parse().ok()?, name)
}

/// `text` with its location lines taken out (outside code fences), and
/// the locations they named.
#[must_use]
pub fn extract(text: &str) -> (String, Vec<Location>) {
    let mut found = Vec::new();
    let mut fenced = false;
    let mut kept = Vec::new();
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        match (!fenced).then(|| directive(line)).flatten() {
            Some(location) => found.push(location),
            None => kept.push(line),
        }
    }
    (kept.join("\n").trim().to_owned(), found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_location_line_is_taken_out_of_the_reply() {
        let (text, found) =
            extract("Meet here:\nLOCATION: -23.5614,-46.6559 Paulista Avenue\nSee you.");
        assert_eq!(text, "Meet here:\nSee you.");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name.as_deref(), Some("Paulista Avenue"));
        let (kept, none) = extract("```\nLOCATION: 1,2\n```");
        assert!(none.is_empty() && kept.contains("LOCATION"), "code stays");
        assert!(directive("LOCATION: 91,0").is_none(), "off the map");
    }

    #[test]
    fn a_pin_round_trips_through_its_attachment() {
        let pin = Location::new(-23.5, -46.6, Some("Office".into())).unwrap();
        let back = Location::parse(&pin.attachment().path).unwrap();
        assert_eq!(back, pin);
        assert!((Location::parse("geo:10,20;u=35").unwrap().longitude - 20.0).abs() < f64::EPSILON);
        assert!(pin.describe(true).starts_with("[Live location: Office"));
        assert!(pin.link().contains("mlat=-23.5"));
    }
}
