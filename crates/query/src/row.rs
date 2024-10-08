use crate::serializer::RowSerializer;
use schemajs_primitives::column::types::DataValue;
use schemajs_primitives::column::Column;
use std::hash::Hash;

/// The `Row` trait defines the core operations that any row in the database must implement.
/// It extends the `RowSerializer` trait to ensure that rows can be serialized and deserialized
/// and also requires implementing the `From<Vec<u8>>` trait for construction from a byte slice.
///
/// The generic type `T` represents the type of data or row that this trait is being implemented for.
///
/// # Required Methods:
/// - `get_value`: Retrieves the value of a specific column from the row, returning `Option<DataValue>`.
/// - `get_table_name`: Returns the name of the table to which the row belongs as a `String`.
/// - `validate`: Validates the row, ensuring it adheres to certain rules or constraints, returning a `bool` indicating whether the row is valid.
pub trait Row<T>: RowSerializer<T> + for<'a> From<&'a [u8]> {
    /// Retrieves the value from a specific column in the row.
    ///
    /// # Parameters:
    /// - `column`: A reference to the `Column` for which to get the value.
    ///
    /// # Returns:
    /// - `Option<DataValue>`: The value of the column, if present. If the value is not found, it returns `None`.
    fn get_value(&self, column: &Column) -> Option<DataValue>;

    /// Returns the name of the table to which the row belongs.
    ///
    /// # Returns:
    /// - `String`: The name of the table.
    fn get_table_name(&self) -> String;

    /// Validates the row based on its internal data and constraints. (Such as data types)
    ///
    /// # Returns:
    /// - `bool`: `true` if the row is valid, otherwise `false`.
    fn validate(&self) -> bool;
}
