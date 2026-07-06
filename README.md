# dicom-ducklink

DICOM medical imaging reader for DuckDB, powered by ducklink (WebAssembly Component Model).

## What it does

Load this WASM component into DuckDB via ducklink, then query DICOM files with SQL:

```sql
FROM ducklink_load('dicom_reader');

-- Single file
SELECT PatientName, Modality, Rows, Columns, SliceThickness
FROM read_dicom('/scans/chest_ct.dcm');

-- Whole directory
SELECT Modality, count(*) AS studies
FROM read_dicom_dir('/scans/2024/*.dcm')
GROUP BY Modality
ORDER BY studies DESC;
```

Extracts 19 DICOM tags: PatientName, PatientID, PatientBirthDate, PatientSex, StudyDate, Modality, StudyDescription, InstitutionName, Rows, Columns, BitsAllocated, BitsStored, SamplesPerPixel, SliceThickness, InstanceNumber, SeriesInstanceUID, StudyInstanceUID, SOPInstanceUID, PixelData (BLOB).

## How it works

```
DuckDB SQL Query
  → ducklink (embeds wasmtime)
    → DICOM Reader .wasm (this component)
      → dicom-rs parses DICOM tags
        → returns rows back to DuckDB
```

The component is built **once** to `.wasm` and runs on every platform DuckDB supports — native, standalone WASM, and in-browser.

## Build

```bash
# 1. Clone ducklink monorepo
git clone --recurse-submodules https://github.com/tegmentum/ducklink.git

# 2. Place this repo as an extension
ln -s $(pwd) ducklink/extensions/dicom-reader

# 3. Build the WASM component
cd ducklink/extensions/dicom-reader
cargo component build --target wasm32-wasip2 --release

# 4. The output is in target/wasm32-wasip2/release/dicom_reader_component.wasm
```

## Standalone verification (no ducklink needed)

The core DICOM reading logic is verified independently:

```bash
cd standalone-demo
cargo run
```

This creates and reads an in-memory DICOM object using dicom-rs 0.6, proving:
- `InMemDicomObject` API works for tag extraction
- Cross-compiles to `wasm32-wasip2` target
- All 19 DICOM tags parse correctly

## Status

- ✅ dicom-rs 0.6 verified (in-memory, cross-compiles to wasm32-wasip2)
- ✅ WIT interface files in place (duckdb:extension world v4)
- ✅ Component source complete (registration + dispatch + tag extraction)
- ⏳ Needs ducklink monorepo submodules for full `cargo component build`
- ⏳ Needs end-to-end smoke test against real ducklink-extension

## License

MIT
