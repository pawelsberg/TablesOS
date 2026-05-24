//! Test-data seeder for the TablesOS volume.
//!
//! Builds a small but representative relational dataset — four tables wired
//! together by foreign keys — so the GUI can be exercised against real data.
//! It is pure engine API (`tablestore`), so it runs against any [`BlockDevice`]:
//! the image builder calls it on the in-memory volume it formats, before that
//! volume is written into the disk image (see `main.rs --seed`).
//!
//! Schema (ids are `NOT NULL UNIQUE`; every `*_id` is a foreign key into
//! `addresses.id`, sharing its `UnsignedInteger` type as the engine requires):
//!
//! ```text
//! addresses(id, name)
//! people   (id, firstname, surname, address_id → addresses.id)
//! distances(id, from_address_id → addresses.id, to_address_id → addresses.id)
//! notes    (id, note, timestamp)
//! ```

use tablestore::schema::Column;
use tablestore::{BlockDevice, Store, StoreError, Type, Value};

/// Row counts produced, for a one-line build summary.
pub struct SeedStats {
    pub addresses: usize,
    pub people: usize,
    pub distances: usize,
    pub notes: usize,
}

const ADDRESSES: usize = 100;
const PEOPLE: usize = 200;
const DISTANCES: usize = 20;
const NOTES: usize = 500;

const FIRST: &[&str] = &[
    "John", "Jane", "Alice", "Bob", "Carol", "David", "Emma", "Frank", "Grace", "Henry", "Ivy",
    "Jack", "Karen", "Leo", "Mia", "Nina", "Oscar", "Paula", "Quinn", "Rosa",
];
const SURNAME: &[&str] = &[
    "Smith", "Jones", "Taylor", "Brown", "Williams", "Wilson", "Davies", "Evans", "Thomas",
    "Roberts", "Walker", "Wright", "Green", "Hall", "Wood", "Clark", "Harris", "Lewis", "Young",
    "King",
];
const STREET: &[&str] = &[
    "High", "Station", "Main", "Church", "Park", "Victoria", "Green", "Manor", "Kings", "Queens",
    "Mill", "School", "Bridge", "North", "South", "West", "East", "Castle", "Market", "Chapel",
];
const SENTENCE: &[&str] = &[
    "follow up next week",
    "left a voicemail",
    "awaiting confirmation",
    "delivery rescheduled",
    "payment received",
    "needs a second review",
    "address verified",
    "marked as urgent",
    "no answer, will retry",
    "archived for reference",
];

/// A non-null unsigned-integer cell.
fn u(n: usize) -> Option<Value> {
    Some(Value::parse(Type::UnsignedInteger, &n.to_string()).expect("unsigned literal"))
}
/// A non-null string cell.
fn s(text: &str) -> Option<Value> {
    Some(Value::Str(text.to_string()))
}
/// A non-null date-time cell from a canonical `YYYY-MM-DDThh:mm:ss` string.
fn dt(text: &str) -> Option<Value> {
    Some(Value::parse(Type::DateTime, text).expect("datetime literal"))
}
/// A non-null decimal cell from a canonical decimal string.
fn dec(text: &str) -> Option<Value> {
    Some(Value::parse(Type::Decimal, text).expect("decimal literal"))
}

/// `NOT NULL UNIQUE` when `unique`, else a plain nullable column.
fn col(name: &str, ty: Type, unique: bool) -> Column {
    Column {
        name: name.to_string(),
        ty,
        nullable: !unique,
        unique,
        display_width: None,
    }
}

/// Give a column an explicit Table Browser display width (characters). Lets the
/// seeded tables ship with sensible widths: narrow `id` columns, and wider
/// foreign-key columns whose cells show a referenced row's label.
fn w(mut c: Column, width: u16) -> Column {
    c.display_width = Some(width);
    c
}

/// Populate `store` with the test dataset. The store must be freshly formatted
/// (the builder formats it immediately before calling this).
pub fn seed<D: BlockDevice>(store: &mut Store<D>) -> Result<SeedStats, StoreError> {
    // --- addresses: the table every foreign key points at -----------------
    store.create_table("addresses")?;
    store.add_column("addresses", w(col("id", Type::UnsignedInteger, true), 6))?;
    store.add_column("addresses", w(col("name", Type::String, false), 22))?;
    // Wherever an address is referenced (e.g. a person's `address_id`), show
    // its name as the label.
    store.set_reference_columns("addresses", vec!["name".to_string()])?;
    for i in 0..ADDRESSES {
        let name = format!("{} {} Road", (i % 50) + 1, STREET[i % STREET.len()]);
        store.insert("addresses", alloc_row([u(i + 1), s(&name)]))?;
    }

    // --- people: one foreign key into addresses ---------------------------
    store.create_table("people")?;
    store.add_column("people", w(col("id", Type::UnsignedInteger, true), 6))?;
    store.add_column("people", w(col("firstname", Type::String, false), 12))?;
    store.add_column("people", w(col("surname", Type::String, false), 12))?;
    // Foreign key: its cells render the address's name label, so give it room.
    store.add_column("people", w(col("address_id", Type::UnsignedInteger, false), 22))?;
    store.add_fk("people", "address_id", "addresses", "id")?;
    // A person is labelled by their full name wherever referenced.
    store.set_reference_columns("people", vec!["firstname".to_string(), "surname".to_string()])?;
    for i in 0..PEOPLE {
        let first = FIRST[i % FIRST.len()];
        let last = SURNAME[(i / FIRST.len()) % SURNAME.len()];
        let address_id = (i * 7) % ADDRESSES + 1;
        store.insert(
            "people",
            alloc_row([u(i + 1), s(first), s(last), u(address_id)]),
        )?;
    }

    // --- distances: two foreign keys into the same target -----------------
    store.create_table("distances")?;
    store.add_column("distances", w(col("id", Type::UnsignedInteger, true), 6))?;
    // Both foreign keys show an address name; the column names are long too.
    store.add_column("distances", w(col("from_address_id", Type::UnsignedInteger, false), 22))?;
    store.add_column("distances", w(col("to_address_id", Type::UnsignedInteger, false), 22))?;
    store.add_column("distances", w(col("distance_miles", Type::Decimal, false), 14))?;
    store.add_fk("distances", "from_address_id", "addresses", "id")?;
    store.add_fk("distances", "to_address_id", "addresses", "id")?;
    // Label a distance by its mileage wherever it is referenced.
    store.set_reference_columns("distances", vec!["distance_miles".to_string()])?;
    for i in 0..DISTANCES {
        // `from` is even-spaced, `to` is offset by an odd stride, so the two
        // never coincide (4i+13 is never ≡ 0 mod 100) — no self-distances.
        let from = (i * 3) % ADDRESSES + 1;
        let to = (i * 7 + 13) % ADDRESSES + 1;
        // A plausible one-decimal-place mileage in 0.5..99.9.
        let tenths = (i * 37 + 5) % 1000;
        let miles = format!("{}.{}", tenths / 10, tenths % 10);
        store.insert("distances", alloc_row([u(i + 1), u(from), u(to), dec(&miles)]))?;
    }

    // --- notes: free text + a timestamp, no references --------------------
    store.create_table("notes")?;
    store.add_column("notes", w(col("id", Type::UnsignedInteger, true), 6))?;
    store.add_column("notes", w(col("note", Type::String, false), 36))?;
    // A full DateTime is 19 chars — the default would clip the seconds.
    store.add_column("notes", w(col("timestamp", Type::DateTime, false), 20))?;
    for i in 0..NOTES {
        let note = format!("Note #{}: {}", i + 1, SENTENCE[i % SENTENCE.len()]);
        // Spread across a few years and the full clock; day ≤ 28 so every
        // month is valid without a calendar lookup.
        let ts = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            2020 + (i % 7),
            (i % 12) + 1,
            (i % 28) + 1,
            i % 24,
            i % 60,
            (i * 7) % 60,
        );
        store.insert("notes", alloc_row([u(i + 1), s(&note), dt(&ts)]))?;
    }

    Ok(SeedStats {
        addresses: ADDRESSES,
        people: PEOPLE,
        distances: DISTANCES,
        notes: NOTES,
    })
}

/// `insert` takes an owned `Vec`; build one from a fixed-size cell array.
fn alloc_row<const N: usize>(cells: [Option<Value>; N]) -> Vec<Option<Value>> {
    cells.into()
}
