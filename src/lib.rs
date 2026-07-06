// dicom-ducklink: DICOM reader for DuckDB via ducklink WASM component.
// Verified: cargo component build --target wasm32-wasip2 --release
//
// SQL usage:
//   FROM ducklink_load('dicom_reader');
//   SELECT PatientName, Modality, Rows, Columns FROM read_dicom('/scans/ct.dcm');

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use dicom::core::header::{DataElementHeader, Length, PrimitiveDataElement};
use dicom::core::value::PrimitiveValue;
use dicom::core::VR;
use dicom::core::Tag;
use dicom::dictionary_std::tags;
use dicom::object::mem::InMemDicomObject;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

use duckdb::extension::column_types as __col;
use duckdb::extension::{runtime, types};
use exports::duckdb::extension::{callback_dispatch, guest};

datalink_extcore::__columnar_bridge_conv!(types, __col);

struct DicomExtension;

impl guest::Guest for DicomExtension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_read_dicom()?;
        Ok(types::Loadresult {
            name: "dicom_reader".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }

    fn reconfigure(_: Vec<String>) -> Result<bool, types::Duckerror> { Ok(false) }
    fn shutdown() -> Result<bool, types::Duckerror> { Ok(false) }
}

impl callback_dispatch::Guest for DicomExtension {
    fn call_scalar_batch_col(
        _: u32, _: Vec<callback_dispatch::Colvec>, _: types::Invokeinfo,
    ) -> Result<callback_dispatch::Colvec, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no scalars".into()))
    }
    fn call_aggregate_col(
        _: u32, _: Vec<callback_dispatch::Colvec>,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no aggregates".into()))
    }
    fn call_cast_col(
        _: u32, _: callback_dispatch::Colvec,
    ) -> Result<callback_dispatch::Colvec, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no casts".into()))
    }
    fn call_scalar(
        _: u32, _: Vec<types::Duckvalue>, _: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no scalars".into()))
    }
    fn call_table(
        handle: u32, args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        let h = table_handlers().lock().map_err(|_| types::Duckerror::Internal("lock".into()))?;
        match h.get(&handle).copied().ok_or_else(|| types::Duckerror::Internal("bad handle".into()))? {
            TableHandler::ReadDicom => read_single_dicom(&args),
        }
    }
    fn call_pragma(_: u32, _: Vec<types::Duckvalue>) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no pragmas".into()))
    }
    fn call_cast(_: u32, _: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no casts".into()))
    }
}

export!(DicomExtension);

// ── Registration ──

fn register_read_dicom() -> Result<(), types::Duckerror> {
    let cap = runtime::get_capability(types::Capabilitykind::Table)
        .ok_or_else(|| types::Duckerror::Internal("no table capability".into()))?;
    let reg = match cap {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("wrong capability".into())),
    };

    let handle = NEXT_TABLE_HANDLE.fetch_add(1, Ordering::Relaxed);
    table_handlers().lock().map_err(|_| types::Duckerror::Internal("lock".into()))?
        .insert(handle, TableHandler::ReadDicom);

    let cb = runtime::TableCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("path".into()), logical: types::Logicaltype::Text,
    }];
    let columns: Vec<types::Columndef> = DICOM_SCHEMA.iter().map(|(n, lt)| types::Columndef {
        name: (*n).into(), logical: lt.clone(),
    }).collect();
    let opts = runtime::Extopts {
        description: Some("Reads DICOM metadata into a table (19 tags)".into()),
        tags: vec!["dicom".into()],
    };
    reg.register("read_dicom", &args, &columns, cb, Some(&opts))?;
    Ok(())
}

// ── DICOM Schema ──

const DICOM_SCHEMA: &[(&str, types::Logicaltype)] = &[
    ("PatientName",        types::Logicaltype::Text),
    ("PatientID",          types::Logicaltype::Text),
    ("PatientBirthDate",   types::Logicaltype::Text),
    ("PatientSex",         types::Logicaltype::Text),
    ("StudyDate",          types::Logicaltype::Text),
    ("Modality",           types::Logicaltype::Text),
    ("StudyDescription",   types::Logicaltype::Text),
    ("InstitutionName",    types::Logicaltype::Text),
    ("Rows",               types::Logicaltype::Int64),
    ("Columns",            types::Logicaltype::Int64),
    ("BitsAllocated",      types::Logicaltype::Int64),
    ("BitsStored",         types::Logicaltype::Int64),
    ("SamplesPerPixel",    types::Logicaltype::Int64),
    ("SliceThickness",     types::Logicaltype::Float64),
    ("InstanceNumber",     types::Logicaltype::Int64),
    ("SeriesInstanceUID",  types::Logicaltype::Text),
    ("StudyInstanceUID",   types::Logicaltype::Text),
    ("SOPInstanceUID",     types::Logicaltype::Text),
    ("PixelData",          types::Logicaltype::Blob),
];

// ── DICOM Reading ──

fn read_single_dicom(args: &[types::Duckvalue]) -> Result<types::Resultset, types::Duckerror> {
    let path = match args.first() {
        Some(types::Duckvalue::Text(p)) => p.clone(),
        _ => return Err(types::Duckerror::Invalidargument("read_dicom(path) — VARCHAR".into())),
    };
    let obj = dicom::object::open_file(std::path::Path::new(&path))
        .map_err(|e| types::Duckerror::Internal(format!("DICOM parse: {e}")))?;
    Ok(vec![extract_row(&obj)])
}

fn extract_row(obj: &dicom::object::FileDicomObject<InMemDicomObject>) -> Vec<types::Duckvalue> {
    let text = |tag: Tag| -> Option<String> {
        obj.element(tag).ok().and_then(|e| {
            format!("{:?}", e.value()).into()
        }).or_else(|| Some(String::new()))
        .filter(|s| !s.is_empty() && s != "Primitive(Str(\"\"))")
    };

    let int16 = |tag: Tag| -> Option<i64> {
        obj.element(tag).ok().and_then(|e| {
            let s = format!("{:?}", e.value());
            s.split(['[', ']']).nth(1).and_then(|n| n.parse::<i64>().ok())
        })
    };

    let float_ds = |tag: Tag| -> Option<f64> {
        text(tag).and_then(|s| s.split('"').nth(1).and_then(|n| n.parse::<f64>().ok()))
    };

    vec![
        text(tags::PATIENT_NAME).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(tags::PATIENT_ID).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(tags::PATIENT_BIRTH_DATE).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(tags::PATIENT_SEX).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(tags::STUDY_DATE).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(tags::MODALITY).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(tags::STUDY_DESCRIPTION).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(tags::INSTITUTION_NAME).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        int16(Tag(0x0028, 0x0010)).map(types::Duckvalue::Int64).unwrap_or(types::Duckvalue::Null),
        int16(Tag(0x0028, 0x0011)).map(types::Duckvalue::Int64).unwrap_or(types::Duckvalue::Null),
        int16(Tag(0x0028, 0x0100)).map(types::Duckvalue::Int64).unwrap_or(types::Duckvalue::Null),
        int16(Tag(0x0028, 0x0101)).map(types::Duckvalue::Int64).unwrap_or(types::Duckvalue::Null),
        int16(Tag(0x0028, 0x0002)).map(types::Duckvalue::Int64).unwrap_or(types::Duckvalue::Null),
        float_ds(Tag(0x0018, 0x0050)).map(types::Duckvalue::Float64).unwrap_or(types::Duckvalue::Null),
        int16(Tag(0x0020, 0x0013)).map(types::Duckvalue::Int64).unwrap_or(types::Duckvalue::Null),
        text(Tag(0x0020, 0x000E)).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(Tag(0x0020, 0x000D)).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        text(Tag(0x0008, 0x0018)).map(types::Duckvalue::Text).unwrap_or(types::Duckvalue::Null),
        types::Duckvalue::Null,  // PixelData — skip blob for now
    ]
}

// ── Handler registry ──

#[derive(Clone, Copy)]
enum TableHandler { ReadDicom }

static NEXT_TABLE_HANDLE: AtomicU32 = AtomicU32::new(1);
static TABLE_HANDLERS: OnceLock<Mutex<HashMap<u32, TableHandler>>> = OnceLock::new();

fn table_handlers() -> &'static Mutex<HashMap<u32, TableHandler>> {
    TABLE_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
