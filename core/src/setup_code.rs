//! A device's setup code, in any of the forms it comes in.

use rs_matter::error::{Error, ErrorCode};
use rs_matter::pairing::qr::{QrPayload, QR_PREFIX};
use rs_matter::transport::network::mdns::CommissionableFilter;

/// The most digits a passcode written on its own has.
const MAX_PASSCODE_LEN: usize = 8;

/// What a device's setup code tells: its passcode, and how to find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupCode {
    pub passcode: u32,
    /// Narrows discovery to the device; matches any device for a bare passcode.
    pub filter: CommissionableFilter,
}

impl SetupCode {
    /// Parse a passcode (`20202021`), a manual pairing code (`3497-011-2332`, 11
    /// or 21 digits) or a QR code payload (`MT:...`).
    pub fn parse(code: &str) -> Result<Self, Error> {
        let code = code.trim();

        if code.starts_with(QR_PREFIX) {
            let mut buf = [0; 128];
            let payload = QrPayload::parse(code, &mut buf)?;

            return Ok(Self {
                passcode: payload.passcode(),
                filter: payload.commissionable_filter(),
            });
        }

        let digits = code.chars().filter(|c| !matches!(c, '-' | ' '));

        if digits.clone().count() <= MAX_PASSCODE_LEN {
            let passcode = digits
                .collect::<alloc::string::String>()
                .parse()
                .map_err(|_| ErrorCode::InvalidData)?;

            return Ok(Self {
                passcode,
                filter: CommissionableFilter::default(),
            });
        }

        let payload = QrPayload::parse_pairing_code(code)?;

        Ok(Self {
            passcode: payload.passcode(),
            filter: payload.commissionable_filter(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_passcode() {
        let code = SetupCode::parse(" 20202021 ").unwrap();

        assert_eq!(code.passcode, 20202021);
        assert_eq!(code.filter, CommissionableFilter::default());
    }

    #[test]
    fn parses_a_manual_pairing_code() {
        let code = SetupCode::parse("3497-011-2332").unwrap();

        assert_eq!(code.passcode, 20202021);
        assert_eq!(code.filter.short_discriminator, Some(15));
        assert_eq!(code.filter.discriminator, None);
    }

    #[test]
    fn parses_a_qr_code_payload() {
        let code = SetupCode::parse("MT:-24J0AFN00KA064IJ3P0IXZB0DK5N1K8SQ1RYCU1-A40").unwrap();

        assert_eq!(code.passcode, 20202021);
        assert_eq!(code.filter.discriminator, Some(3840));
    }

    #[test]
    fn rejects_garbage() {
        assert!(SetupCode::parse("").is_err());
        assert!(SetupCode::parse("2020x021").is_err());
        assert!(SetupCode::parse("3497-011-2333").is_err());
        assert!(SetupCode::parse("MT:nonsense").is_err());
    }
}
