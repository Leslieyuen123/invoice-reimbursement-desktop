use crate::domain::error::AppError;

pub const MAX_SAFE_AMOUNT_CENTS: i64 = 9_007_199_254_740_991;

pub fn validate_amount_cents(value: i64, field: &str) -> Result<i64, AppError> {
    if value < 0 {
        return Err(AppError::validation(field, "amount must be nonnegative"));
    }
    if value > MAX_SAFE_AMOUNT_CENTS {
        return Err(AppError::validation(
            field,
            "amount exceeds the JavaScript safe integer range",
        ));
    }
    Ok(value)
}

pub fn validate_optional_amount_cents(
    value: Option<i64>,
    field: &str,
) -> Result<Option<i64>, AppError> {
    value
        .map(|value| validate_amount_cents(value, field))
        .transpose()
}

pub fn checked_add_amount_cents(total: i64, amount: i64, field: &str) -> Result<i64, AppError> {
    validate_amount_cents(total, field)?;
    validate_amount_cents(amount, field)?;
    total
        .checked_add(amount)
        .ok_or_else(|| amount_total_out_of_range(field))
        .and_then(|sum| validate_amount_cents(sum, field))
}

pub fn amount_total_out_of_range(field: &str) -> AppError {
    AppError::validation(
        field,
        "amount total exceeds the JavaScript safe integer range",
    )
}
