use alloc::{format, string::String, vec::Vec};
use core::{
    array::TryFromSliceError,
    convert::TryFrom,
    fmt::{self, Debug, Display, Formatter},
    num::ParseIntError,
};

use hex_fmt::HexFmt;

use crate::{bytesrepr, AccessRights, ApiError, Key, ACCESS_RIGHTS_SERIALIZED_LENGTH};

/// The number of bytes in a [`URef`] address.
pub const UREF_ADDR_LENGTH: usize = 32;

/// The number of bytes in a serialized [`URef`] where the [`AccessRights`] are not `None`.
pub const UREF_SERIALIZED_LENGTH: usize = UREF_ADDR_LENGTH + ACCESS_RIGHTS_SERIALIZED_LENGTH;

const PREFIX: &str = "uref-";

/// The address of a [`URef`](types::URef) (unforgeable reference) on the network.
pub type URefAddr = [u8; UREF_ADDR_LENGTH];

#[derive(Debug)]
pub enum FromStrError {
    InvalidPrefix,
    MissingSuffix,
    InvalidAccessRights,
    Hex(base16::DecodeError),
    Int(ParseIntError),
    Address(TryFromSliceError),
}

impl From<base16::DecodeError> for FromStrError {
    fn from(error: base16::DecodeError) -> Self {
        FromStrError::Hex(error)
    }
}

impl From<ParseIntError> for FromStrError {
    fn from(error: ParseIntError) -> Self {
        FromStrError::Int(error)
    }
}

impl From<TryFromSliceError> for FromStrError {
    fn from(error: TryFromSliceError) -> Self {
        FromStrError::Address(error)
    }
}

/// Represents an unforgeable reference, containing an address in the network's global storage and
/// the [`AccessRights`] of the reference.
///
/// A `URef` can be used to index entities such as [`CLValue`](crate::CLValue)s, or smart contracts.
#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct URef(URefAddr, AccessRights);

impl URef {
    /// Constructs a [`URef`] from an address and access rights.
    pub fn new(address: URefAddr, access_rights: AccessRights) -> Self {
        URef(address, access_rights)
    }

    /// Returns the address of this [`URef`].
    pub fn addr(&self) -> URefAddr {
        self.0
    }

    /// Returns the access rights of this [`URef`].
    pub fn access_rights(&self) -> AccessRights {
        self.1
    }

    /// Returns a new [`URef`] with the same address and updated access rights.
    pub fn with_access_rights(self, access_rights: AccessRights) -> Self {
        URef(self.0, access_rights)
    }

    /// Removes the access rights from this [`URef`].
    pub fn remove_access_rights(self) -> Self {
        URef(self.0, AccessRights::NONE)
    }

    /// Returns `true` if the access rights are `Some` and
    /// [`is_readable`](AccessRights::is_readable) is `true` for them.
    pub fn is_readable(self) -> bool {
        self.1.is_readable()
    }

    /// Returns a new [`URef`] with the same address and [`AccessRights::READ`] permission.
    pub fn into_read(self) -> URef {
        URef(self.0, AccessRights::READ)
    }

    /// Returns a new [`URef`] with the same address and [`AccessRights::READ_ADD_WRITE`]
    /// permission.
    pub fn into_read_add_write(self) -> URef {
        URef(self.0, AccessRights::READ_ADD_WRITE)
    }

    /// Returns `true` if the access rights are `Some` and
    /// [`is_writeable`](AccessRights::is_writeable) is `true` for them.
    pub fn is_writeable(self) -> bool {
        self.1.is_writeable()
    }

    /// Returns `true` if the access rights are `Some` and [`is_addable`](AccessRights::is_addable)
    /// is `true` for them.
    pub fn is_addable(self) -> bool {
        self.1.is_addable()
    }

    /// Formats the address and access rights of the [`URef`] in an unique way that could be used as
    /// a name when storing the given `URef` in a global state.
    pub fn to_formatted_string(&self) -> String {
        // Extract bits as numerical value, with no flags marked as 0.
        let access_rights_bits = self.access_rights().bits();
        // Access rights is represented as octal, which means that max value of u8 can
        // be represented as maximum of 3 octal digits.
        format!(
            "{}{}-{:03o}",
            PREFIX,
            base16::encode_lower(&self.addr()),
            access_rights_bits
        )
    }

    /// Parses a string formatted as per `Self::as_string()` into a `URef`.
    pub fn from_formatted_str(input: &str) -> Result<Self, FromStrError> {
        let remainder = input
            .strip_prefix(PREFIX)
            .ok_or_else(|| FromStrError::InvalidPrefix)?;
        let parts = remainder.splitn(2, '-').collect::<Vec<_>>();
        if parts.len() != 2 {
            return Err(FromStrError::MissingSuffix);
        }
        let addr = URefAddr::try_from(base16::decode(parts[0])?.as_ref())?;
        let access_rights_value = u8::from_str_radix(parts[1], 8)?;
        let access_rights = AccessRights::from_bits(access_rights_value)
            .ok_or_else(|| FromStrError::InvalidAccessRights)?;
        Ok(URef(addr, access_rights))
    }
}

impl Display for URef {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let addr = self.addr();
        let access_rights = self.access_rights();
        write!(f, "URef({}, {})", HexFmt(&addr), access_rights)
    }
}

impl Debug for URef {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}

impl bytesrepr::ToBytes for URef {
    fn to_bytes(&self) -> Result<Vec<u8>, bytesrepr::Error> {
        let mut result = bytesrepr::unchecked_allocate_buffer(self);
        result.append(&mut self.0.to_bytes()?);
        result.append(&mut self.1.to_bytes()?);
        Ok(result)
    }

    fn serialized_length(&self) -> usize {
        UREF_SERIALIZED_LENGTH
    }
}

impl bytesrepr::FromBytes for URef {
    fn from_bytes(bytes: &[u8]) -> Result<(Self, &[u8]), bytesrepr::Error> {
        let (id, rem): ([u8; 32], &[u8]) = bytesrepr::FromBytes::from_bytes(bytes)?;
        let (access_rights, rem): (AccessRights, &[u8]) = bytesrepr::FromBytes::from_bytes(rem)?;
        Ok((URef(id, access_rights), rem))
    }
}

impl TryFrom<Key> for URef {
    type Error = ApiError;

    fn try_from(key: Key) -> Result<Self, Self::Error> {
        if let Key::URef(uref) = key {
            Ok(uref)
        } else {
            Err(ApiError::UnexpectedKeyVariant)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uref_as_string() {
        // Since we are putting URefs to named_keys map keyed by the label that
        // `as_string()` returns, any changes to the string representation of
        // that type cannot break the format.
        let addr_array = [0u8; 32];
        let uref_a = URef::new(addr_array, AccessRights::READ);
        assert_eq!(
            uref_a.to_formatted_string(),
            "uref-0000000000000000000000000000000000000000000000000000000000000000-001"
        );
        let uref_b = URef::new(addr_array, AccessRights::WRITE);
        assert_eq!(
            uref_b.to_formatted_string(),
            "uref-0000000000000000000000000000000000000000000000000000000000000000-002"
        );

        let uref_c = uref_b.remove_access_rights();
        assert_eq!(
            uref_c.to_formatted_string(),
            "uref-0000000000000000000000000000000000000000000000000000000000000000-000"
        );
    }

    fn round_trip(uref: URef) {
        let string = uref.to_formatted_string();
        let parsed_uref = URef::from_formatted_str(&string).unwrap();
        assert_eq!(uref, parsed_uref);
    }

    #[test]
    fn uref_from_str() {
        round_trip(URef::new([0; 32], AccessRights::NONE));
        round_trip(URef::new([255; 32], AccessRights::READ_ADD_WRITE));

        let invalid_prefix =
            "ref-0000000000000000000000000000000000000000000000000000000000000000-000";
        assert!(URef::from_formatted_str(invalid_prefix).is_err());

        let invalid_prefix =
            "uref0000000000000000000000000000000000000000000000000000000000000000-000";
        assert!(URef::from_formatted_str(invalid_prefix).is_err());

        let short_addr = "uref-00000000000000000000000000000000000000000000000000000000000000-000";
        assert!(URef::from_formatted_str(short_addr).is_err());

        let long_addr =
            "uref-000000000000000000000000000000000000000000000000000000000000000000-000";
        assert!(URef::from_formatted_str(long_addr).is_err());

        let invalid_hex =
            "uref-000000000000000000000000000000000000000000000000000000000000000g-000";
        assert!(URef::from_formatted_str(invalid_hex).is_err());

        let invalid_suffix_separator =
            "uref-0000000000000000000000000000000000000000000000000000000000000000:000";
        assert!(URef::from_formatted_str(invalid_suffix_separator).is_err());

        let invalid_suffix =
            "uref-0000000000000000000000000000000000000000000000000000000000000000-abc";
        assert!(URef::from_formatted_str(invalid_suffix).is_err());

        let invalid_access_rights =
            "uref-0000000000000000000000000000000000000000000000000000000000000000-200";
        assert!(URef::from_formatted_str(invalid_access_rights).is_err());
    }
}
