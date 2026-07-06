// dicom-ducklink: DICOM metadata reader as a DuckDB table function via ducklink
//
// Load in DuckDB:
//   FROM ducklink_load('dicom_reader');
//   SELECT PatientName, Modality, Rows, Columns, SliceThickness
//   FROM read_dicom('/scans/chest_ct.dcm');
//
// Build:
//   1. Clone ducklink monorepo: git clone --recurse-submodules https://github.com/tegmentum/ducklink.git
//   2. Place this component under ducklink/extensions/dicom-reader/
//   3. cargo component build --target wasm32-wasip2 --release
//   4. The output .wasm is in target/wasm32-wasip2/release/

mod bridge;

use std::collections::HashMap;
use std::sync::{atomic::AtomicU32, Mutex, OnceLock};
use std::sync::atomic::Ordering;

use bridge::*;

use dicom::core::Tag;
use dicom::core::header::{DataElementHeader, HasLength, Length, PrimitiveDataElement};
use dicom::core::value::PrimitiveValue;
use dicom::core::VR;
use dicom::dictionary_std::tags;
use dicom::object::mem::InMemDicomObject;

wit_bindgen::generate!({
    path: "./wit",
    world: "duckdb:extension/duckdb-extension",
});

use duckdb::extension::{catalog, files, runtime, types};
use duckdb::extension::column_types as __col;
use exports::duckdb::extension::{callback_dispatch, guest};

datalink_extcore::__columnar_bridge_conv!(types, __col);

struct DicomExtension;

impl guest::Guest for DicomExtension {
    fn load() -> Result<types::Loadresult, types::Duckerror> {
        register_read_dicom()?;
        register_read_dicom_dir()?;
        Ok(types::Loadresult {
            name: "dicom_reader".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            requires: Vec::new().into(),
        })
    }

    fn reconfigure(_keys: Vec<String>) -> Result<bool, types::Duckerror> {
        Ok(false)
    }

    fn shutdown() -> Result<bool, types::Duckerror> {
        Ok(false)
    }
}

impl callback_dispatch::Guest for DicomExtension {
    fn call_scalar_batch_col(
        _handle: u32,
        _args: Vec<callback_dispatch::Colvec>,
        _ctx: types::Invokeinfo,
    ) -> Result<callback_dispatch::Colvec, types::Duckerror> {
        // No scalar functions registered — dicom_reader is table-only
        Err(types::Duckerror::Unsupported("no scalars".into()))
    }

    fn call_aggregate_col(
        _handle: u32,
        _args: Vec<callback_dispatch::Colvec>,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no aggregates".into()))
    }

    fn call_cast_col(
        _handle: u32,
        _arg: callback_dispatch::Colvec,
    ) -> Result<callback_dispatch::Colvec, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no casts".into()))
    }

    fn call_scalar(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
        _ctx: types::Invokeinfo,
    ) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no scalars".into()))
    }

    fn call_table(
        handle: u32,
        args: Vec<types::Duckvalue>,
    ) -> Result<types::Resultset, types::Duckerror> {
        let handler = table_handlers()
            .lock()
            .map_err(|_| types::Duckerror::Internal("lock poisoned".into()))?
            .get(&handle)
            .copied()
            .ok_or_else(|| types::Duckerror::Internal("unknown table handle".into()))?;

        match handler {
            TableHandler::ReadDicom => read_single_dicom(&args),
            TableHandler::ReadDicomDir => read_dicom_dir(&args),
        }
    }

    fn call_pragma(
        _handle: u32,
        _args: Vec<types::Duckvalue>,
    ) -> Result<Option<types::Duckvalue>, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no pragmas".into()))
    }

    fn call_cast(_handle: u32, _value: types::Duckvalue) -> Result<types::Duckvalue, types::Duckerror> {
        Err(types::Duckerror::Unsupported("no casts".into()))
    }
}

export!(DicomExtension);

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

fn register_read_dicom() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Table).ok_or_else(|| {
        types::Duckerror::Internal("host did not expose table capability".into())
    })?;
    let registry = match capability {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("unexpected capability variant".into())),
    };

    let handle = NEXT_TABLE_HANDLE.fetch_add(1, Ordering::Relaxed);
    table_handlers()
        .lock()
        .map_err(|_| types::Duckerror::Internal("lock poisoned".into()))?
        .insert(handle, TableHandler::ReadDicom);

    let callback = runtime::TableCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("path".into()),
        logical: types::Logicaltype::Text,
    }];
    let columns = DICOM_SCHEMA
        .iter()
        .map(|(name, lt)| types::Columndef {
            name: (*name).into(),
            logical: lt.clone(),
        })
        .collect();

    let opts = runtime::Extopts {
        description: Some("Reads DICOM metadata into a table. Returns one row per file with patient info, study details, and image parameters.".into()),
        tags: vec!["dicom".into(), "medical".into(), "imaging".into()],
    };

    registry.register("read_dicom", &args, &columns, callback, Some(&opts))?;
    Ok(())
}

fn register_read_dicom_dir() -> Result<(), types::Duckerror> {
    let capability = runtime::get_capability(types::Capabilitykind::Table).ok_or_else(|| {
        types::Duckerror::Internal("host did not expose table capability".into())
    })?;
    let registry = match capability {
        runtime::Capability::Table(r) => r,
        _ => return Err(types::Duckerror::Internal("unexpected".into())),
    };

    let handle = NEXT_TABLE_HANDLE.fetch_add(1, Ordering::Relaxed);
    table_handlers()
        .lock()
        .map_err(|_| types::Duckerror::Internal("lock poisoned".into()))?
        .insert(handle, TableHandler::ReadDicomDir);

    let callback = runtime::TableCallback::new(handle);
    let args = vec![runtime::Funcarg {
        name: Some("pattern".into()),
        logical: types::Logicaltype::Text,
    }];
    let columns = DICOM_SCHEMA
        .iter()
        .map(|(name, lt)| types::Columndef {
            name: (*name).into(),
            logical: lt.clone(),
        })
        .chain(std::iter::once(types::Columndef {
            name: "file_path".into(),
            logical: types::Logicaltype::Text,
        }))
        .collect();

    let opts = runtime::Extopts {
        description: Some("Reads DICOM metadata from all files matching a glob pattern. Returns file_path as an extra column.".into()),
        tags: vec!["dicom".into(), "medical".into()],
    };

    registry.register("read_dicom_dir", &args, &columns, callback, Some(&opts))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// DICOM Schema — the 19 tags we extract
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// DICOM Reading Logic
// ---------------------------------------------------------------------------
fn read_single_dicom(args: &[types::Duckvalue]) -> Result<types::Resultset, types::Duckerror> {
    let path = match args.first() {
        Some(types::Duckvalue::Text(p)) => p.clone(),
        _ => return Err(types::Duckerror::Invalidargument("read_dicom(path) expects a VARCHAR path".into())),
    };

    let data = std::fs::read(&path)
        .map_err(|e| types::Duckerror::Internal(format!("cannot read {}: {}", path, e)))?;

    let obj = match dicom::object::open_file(std::path::Path::new(&path)) {
        Ok(o) => o,
        Err(e) => return Err(types::Duckerror::Internal(format!("DICOM parse error: {}", e))),
    };

    Ok(vec![extract_dicom_row(&obj)])
}

fn read_dicom_dir(args: &[types::Duckvalue]) -> Result<types::Resultset, types::Duckerror> {
    let pattern = match args.first() {
        Some(types::Duckvalue::Text(p)) => p.clone(),
        _ => return Err(types::Duckerror::Invalidargument("read_dicom_dir(pattern) expects a VARCHAR glob".into())),
    };

    let paths: Vec<std::path::PathBuf> = glob::glob(&pattern)
        .map_err(|e| types::Duckerror::Internal(format!("glob error: {}", e)))?
        .filter_map(|r| r.ok())
        .collect();

    let mut rows = Vec::with_capacity(paths.len());
    for path in &paths {
        if let Ok(obj) = dicom::object::open_file(path) {
            let mut row = extract_dicom_row(&obj);
            row.push(types::Duckvalue::Text(path.to_string_lossy().into_owned()));
            rows.push(row);
        }
    }
    Ok(rows)
}

fn extract_dicom_row(obj: &dicom::object::FileDicomObject<InMemDicomObject>) -> Vec<types::Duckvalue> {
    let text = |obj: &dicom::object::FileDicomObject<InMemDicomObject>, tag: Tag| -> types::Duckvalue {
        obj.element(tag)
            .ok()
            .and_then(|el| el.to_str().ok())
            .map(|s| types::Duckvalue::Text(s.to_string()))
            .unwrap_or(types::Duckvalue::Null)
    };

    let int = |obj, tag: Tag| -> types::Duckvalue {
        obj.element(tag)
            .ok()
            .and_then(|el| {
                el.value().to_multi_str::<i64>(0).ok()
            })
            .map(types::Duckvalue::Int64)
            .unwrap_or(types::Duckvalue::Null)
    };

    let float = |obj, tag: Tag| -> types::Duckvalue {
        obj.element(tag)
            .ok()
            .and_then(|el| {
                el.value().to_multi_str::<f64>(0).ok()
            })
            .map(types::Duckvalue::Float64)
            .unwrap_or(types::Duckvalue::Null)
    };

    let blob = |obj, tag: Tag| -> types::Duckvalue {
        obj.element(tag)
            .ok()
            .map(|el| {
                let data = el.value().bytes().to_vec();
                types::Duckvalue::Blob(data)
            })
            .unwrap_or(types::Duckvalue::Null)
    };

    vec![
        text(obj, tags::PATIENT_NAME),
        text(obj, tags::PATIENT_ID),
        text(obj, tags::PATIENT_BIRTH_DATE),
        text(obj, tags::PATIENT_SEX),
        text(obj, tags::STUDY_DATE),
        text(obj, tags::MODALITY),
        text(obj, tags::STUDY_DESCRIPTION),
        text(obj, tags::INSTITUTION_NAME),
        int(obj, Tag(0x0028, 0x0010)),   // Rows
        int(obj, Tag(0x0028, 0x0011)),   // Columns
        int(obj, Tag(0x0028, 0x0100)),   // BitsAllocated
        int(obj, Tag(0x0028, 0x0101)),   // BitsStored
        int(obj, Tag(0x0028, 0x0002)),   // SamplesPerPixel
        float(obj, Tag(0x0018, 0x0050)), // SliceThickness
        int(obj, Tag(0x0020, 0x0013)),   // InstanceNumber
        text(obj, Tag(0x0020, 0x000E)),  // SeriesInstanceUID
        text(obj, Tag(0x0020, 0x000D)),  // StudyInstanceUID
        text(obj, Tag(0x0008, 0x0018)),  // SOPInstanceUID
        blob(obj, Tag(0x7FE0, 0x0010)),  // PixelData
    ]
}

// ---------------------------------------------------------------------------
// Handler registry
// ---------------------------------------------------------------------------
#[derive(Clone, Copy)]
enum TableHandler {
    ReadDicom,
    ReadDicomDir,
}

static NEXT_TABLE_HANDLE: AtomicU32 = AtomicU32::new(1);
static TABLE_HANDLERS: OnceLock<Mutex<HashMap<u32, TableHandler>>> = OnceLock::new();

fn table_handlers() -> &'static Mutex<HashMap<u32, TableHandler>> {
    TABLE_HANDLERS.get_or_init(|| Mutex::new(HashMap::new()))
}
