//! Read only the top-level, provider-declared schema of a live ETW record.

use crate::survey::Field;
use std::io;
use std::mem::{offset_of, size_of};
use std::ptr::null;
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows_sys::Win32::System::Diagnostics::Etw::{
    EVENT_PROPERTY_INFO, EVENT_RECORD, PropertyHasCustomSchema, PropertyStruct, TRACE_EVENT_INFO,
    TdhGetEventInformation,
};

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_FIELDS: usize = 64;
const MAX_NAME_UNITS: usize = 128;

fn invalid_schema() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or oversized ETW schema",
    )
}

fn field_name(bytes: &[u8], offset: usize) -> io::Result<String> {
    if !offset.is_multiple_of(2) {
        return Err(invalid_schema());
    }
    let name = bytes.get(offset..).ok_or_else(invalid_schema)?;
    let mut units = [0; MAX_NAME_UNITS];
    for (i, pair) in name
        .as_chunks::<2>()
        .0
        .iter()
        .take(MAX_NAME_UNITS)
        .enumerate()
    {
        let unit = u16::from_le_bytes(*pair);
        if unit == 0 && i != 0 {
            return String::from_utf16(&units[..i]).map_err(|_| invalid_schema());
        }
        if unit == 0 {
            break;
        }
        units[i] = unit;
    }
    Err(invalid_schema())
}

pub fn fields(record: &EVENT_RECORD) -> io::Result<Vec<Field>> {
    let mut size = 0;
    // SAFETY: The event remains live in its ETW callback; this call only queries the output size.
    let status =
        unsafe { TdhGetEventInformation(record, 0, null(), std::ptr::null_mut(), &mut size) };
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let properties_start = offset_of!(TRACE_EVENT_INFO, EventPropertyInfoArray);
    let requested = size as usize;
    if !(properties_start..=MAX_SCHEMA_BYTES).contains(&requested) {
        return Err(invalid_schema());
    }
    let mut buffer = vec![0u64; requested.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<TRACE_EVENT_INFO>();
    // SAFETY: The aligned allocation covers the requested number of bytes.
    let status = unsafe { TdhGetEventInformation(record, 0, null(), info, &mut size) };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let used = size as usize;
    if !(properties_start..=requested).contains(&used) {
        return Err(invalid_schema());
    }
    // SAFETY: The header fields precede the checked end of the returned header.
    let (count, all_properties) = unsafe {
        (
            (*info).TopLevelPropertyCount as usize,
            (*info).PropertyCount as usize,
        )
    };
    let property_size = size_of::<EVENT_PROPERTY_INFO>();
    if count > MAX_FIELDS
        || count > all_properties
        || count > (used - properties_start) / property_size
    {
        return Err(invalid_schema());
    }
    // SAFETY: buffer owns at least used bytes, all initialized before the TDH call.
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), used) };
    let mut fields = Vec::with_capacity(count);
    for index in 0..count {
        let offset = properties_start + index * property_size;
        // SAFETY: The count check above bounds each fixed-size element within the allocation.
        let property = unsafe {
            bytes
                .as_ptr()
                .add(offset)
                .cast::<EVENT_PROPERTY_INFO>()
                .read_unaligned()
        };
        let name = field_name(bytes, property.NameOffset as usize)?;
        let (in_type, out_type) = if property.Flags & PropertyStruct != 0 {
            (0, 0)
        } else if property.Flags & PropertyHasCustomSchema != 0 {
            // SAFETY: The custom schema flag selects this union variant.
            let types = unsafe { property.Anonymous1.customSchemaType };
            (types.InType, types.OutType)
        } else {
            // SAFETY: Scalar properties use the non-struct variant.
            let types = unsafe { property.Anonymous1.nonStructType };
            (types.InType, types.OutType)
        };
        fields.push(Field {
            name,
            in_type,
            out_type,
        });
    }
    Ok(fields)
}
