//! Transaction-log decode (spec §1.5): log lines with prefix
//! `Program data: ` carry base64-encoded tag-prefixed Pod reports
//! (drv-models `log.rs`; reference decoder kit `logs-decoder.ts`).
//! `DeriverseLogEvent` is the tag union over the §1.5 report structs.
//! Unknown tags and non-report lines return `None` (the venue logs many
//! other tags; only the §1.5 set is adapter-relevant). All ids are integer
//! types end to end — nothing routes through f64 (Invariant 8).

use base64::Engine;
use drv_models::log::{
    log_type, PerpChangeLeverageReport, PerpFeesReport, PerpFillOrderReport, PerpFundingReport,
    PerpMassCancelReport, PerpNewOrderReport, PerpOrderCancelReport, PerpOrderRevokeReport,
    PerpPlaceOrderReport, SpotFillOrderReport, SpotOrderCancelReport, SpotPlaceOrderReport,
};

/// Prefix of a program-data log line.
pub const PROGRAM_DATA_PREFIX: &str = "Program data: ";

/// Tag union over the §1.5 report structs.
#[derive(Clone, Copy)]
pub enum DeriverseLogEvent {
    SpotPlaceOrder(SpotPlaceOrderReport),      // tag 10
    SpotFillOrder(SpotFillOrderReport),        // tag 11
    SpotOrderCancel(SpotOrderCancelReport),    // tag 13
    PerpPlaceOrder(PerpPlaceOrderReport),      // tag 18
    PerpFillOrder(PerpFillOrderReport),        // tag 19
    PerpNewOrder(PerpNewOrderReport),          // tag 20
    PerpOrderCancel(PerpOrderCancelReport),    // tag 21
    PerpOrderRevoke(PerpOrderRevokeReport),    // tag 22
    PerpFees(PerpFeesReport),                  // tag 23
    PerpFunding(PerpFundingReport),            // tag 24
    PerpMassCancel(PerpMassCancelReport),      // tag 26
    PerpChangeLeverage(PerpChangeLeverageReport), // tag 28
}

impl DeriverseLogEvent {
    /// Per-instrument monotone sequence number (gap ⇒ missed events ⇒
    /// reconcile, spec §1.5). Reports that carry no seq_no return None.
    #[must_use]
    pub fn seq_no(&self) -> Option<i64> {
        match self {
            Self::SpotPlaceOrder(r) => Some(r.seq_no),
            Self::SpotFillOrder(r) => Some(r.seq_no),
            Self::SpotOrderCancel(r) => Some(r.seq_no),
            Self::PerpPlaceOrder(r) => Some(r.seq_no),
            Self::PerpFillOrder(r) => Some(r.seq_no),
            Self::PerpNewOrder(r) => Some(r.seq_no),
            Self::PerpOrderCancel(r) => Some(r.seq_no),
            Self::PerpOrderRevoke(r) => Some(r.seq_no),
            Self::PerpFees(r) => Some(r.seq_no),
            Self::PerpFunding(r) => Some(r.seq_no),
            Self::PerpMassCancel(r) => Some(r.seq_no),
            Self::PerpChangeLeverage(r) => Some(r.seq_no),
        }
    }
}

fn cast<T: bytemuck::Pod>(payload: &[u8]) -> Option<T> {
    // The venue may append fields in later program versions; require AT
    // LEAST the struct size and read the prefix (tag-prefixed Pod layout;
    // copy-out, alignment-proof).
    let need = std::mem::size_of::<T>();
    if payload.len() < need {
        return None;
    }
    bytemuck::try_pod_read_unaligned(&payload[..need]).ok()
}

/// Decode one raw log line. `None` for non-`Program data:` lines, bad
/// base64, empty payloads, and tags outside the §1.5 set.
#[must_use]
pub fn decode_program_data_line(line: &str) -> Option<DeriverseLogEvent> {
    let b64 = line.strip_prefix(PROGRAM_DATA_PREFIX)?;
    let payload = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let tag = *payload.first()?;
    match tag {
        log_type::SPOT_PLACE_ORDER => cast(&payload).map(DeriverseLogEvent::SpotPlaceOrder),
        log_type::SPOT_FILL_ORDER => cast(&payload).map(DeriverseLogEvent::SpotFillOrder),
        log_type::SPOT_ORDER_CANCEL => cast(&payload).map(DeriverseLogEvent::SpotOrderCancel),
        log_type::PERP_PLACE_ORDER => cast(&payload).map(DeriverseLogEvent::PerpPlaceOrder),
        log_type::PERP_FILL_ORDER => cast(&payload).map(DeriverseLogEvent::PerpFillOrder),
        log_type::PERP_NEW_ORDER => cast(&payload).map(DeriverseLogEvent::PerpNewOrder),
        log_type::PERP_ORDER_CANCEL => cast(&payload).map(DeriverseLogEvent::PerpOrderCancel),
        log_type::PERP_ORDER_REVOKE => cast(&payload).map(DeriverseLogEvent::PerpOrderRevoke),
        log_type::PERP_FEES => cast(&payload).map(DeriverseLogEvent::PerpFees),
        log_type::PERP_FUNDING => cast(&payload).map(DeriverseLogEvent::PerpFunding),
        log_type::PERP_MASS_CANCEL => cast(&payload).map(DeriverseLogEvent::PerpMassCancel),
        log_type::PERP_CHANGE_LEVERAGE => {
            cast(&payload).map(DeriverseLogEvent::PerpChangeLeverage)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn line_for<T: bytemuck::Pod>(report: &T) -> String {
        format!(
            "{PROGRAM_DATA_PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(bytemuck::bytes_of(report))
        )
    }

    #[test]
    fn spot_place_order_round_trips() {
        let mut r = SpotPlaceOrderReport::default();
        r.tag = log_type::SPOT_PLACE_ORDER;
        r.side = 0;
        r.order_id = 987_654_321_012_345;
        r.qty = 33_000_000;
        r.price = 152_250_000_000;
        r.seq_no = 77;
        let Some(DeriverseLogEvent::SpotPlaceOrder(d)) = decode_program_data_line(&line_for(&r))
        else {
            panic!("expected SpotPlaceOrder");
        };
        assert_eq!(d.order_id, 987_654_321_012_345);
        assert_eq!(d.seq_no, 77);
    }

    #[test]
    fn spot_fill_and_cancel_round_trip() {
        let mut f = SpotFillOrderReport::default();
        f.tag = log_type::SPOT_FILL_ORDER;
        f.order_id = 42;
        f.seq_no = 78;
        assert!(matches!(
            decode_program_data_line(&line_for(&f)),
            Some(DeriverseLogEvent::SpotFillOrder(d)) if d.order_id == 42
        ));
        let mut c = SpotOrderCancelReport::default();
        c.tag = log_type::SPOT_ORDER_CANCEL;
        c.order_id = 43;
        c.seq_no = 79;
        assert!(matches!(
            decode_program_data_line(&line_for(&c)),
            Some(DeriverseLogEvent::SpotOrderCancel(d)) if d.order_id == 43
        ));
    }

    #[test]
    fn non_report_lines_are_none() {
        assert!(decode_program_data_line("Program log: Instruction: NewSpotOrder").is_none());
        assert!(decode_program_data_line("Program data: !!!not-base64!!!").is_none());
        assert!(decode_program_data_line("Program data: ").is_none());
        // Unknown tag (200) — payload decodes, tag unmatched.
        let unk = format!(
            "{PROGRAM_DATA_PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode([200u8, 0, 0, 0])
        );
        assert!(decode_program_data_line(&unk).is_none());
    }

    #[test]
    fn truncated_payload_is_none_not_panic() {
        let short = format!(
            "{PROGRAM_DATA_PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode([log_type::SPOT_FILL_ORDER, 1, 2])
        );
        assert!(decode_program_data_line(&short).is_none());
    }
}
