#[cfg(test)] use csv::StringRecord;
use lazy_static::lazy_static;
use regex::{Regex, Captures};

use crate::broker_statement::corporate_actions::{CorporateAction, CorporateActionType, StockSplitRatio};
use crate::core::{EmptyResult, GenericResult};
use crate::currency::Cash;
use crate::formatting::format_date;
#[cfg(test)] use crate::types::{Date, DateTime, Decimal};
use crate::util::{self, DecimalRestrictions};

use super::StatementParser;
use super::common::{self, Record, RecordParser, SecurityID, check_volume, parse_symbol};
#[cfg(test)] use super::common::RecordSpec;

pub struct CorporateActionsParser {
    corporate_actions: Vec<CorporateAction>,
}

impl RecordParser for CorporateActionsParser {
    fn skip_totals(&self) -> bool {
        true
    }

    fn parse(&mut self, _parser: &mut StatementParser, record: &Record) -> EmptyResult {
        // Supplementary record for corporate actions like delisting and liquidation
        // (see https://github.com/KonishchevDmitry/investments/issues/69)
        if record.get_value("Report Date")? == "Closed Lot:" {
            return Ok(());
        }

        self.corporate_actions.push(parse(record)?);
        Ok(())
    }
}

impl CorporateActionsParser {
    pub fn new() -> CorporateActionsParser {
        CorporateActionsParser {
            corporate_actions: Vec::new(),
        }
    }

    pub fn commit(self, parser: &mut StatementParser) -> EmptyResult {
        // Here we postprocess parsed corporate actions:
        // * Complex stock splits are represented by two records, so we join them here

        let mut stock_splits = Vec::<CorporateAction>::new();

        for action in self.corporate_actions {
            match action.action {
                CorporateActionType::StockSplit {..} => {
                    if let Some(last) = stock_splits.last() {
                        if action.time == last.time && action.symbol == last.symbol {
                            stock_splits.push(action);
                        } else {
                            parser.statement.corporate_actions.push(join_stock_splits(stock_splits)?);
                            stock_splits = vec![action];
                        }
                    } else {
                        stock_splits.push(action);
                    }
                },
                _ => parser.statement.corporate_actions.push(action),
            }
        }

        if !stock_splits.is_empty() {
            parser.statement.corporate_actions.push(join_stock_splits(stock_splits)?);
        }

        Ok(())
    }
}

fn parse(record: &Record) -> GenericResult<CorporateAction> {
    let asset_category = record.get_value("Asset Category")?;
    if asset_category != "Stocks" {
        return Err!("Unsupported asset category of corporate action: {:?}", asset_category);
    }

    let time = record.parse_date_time("Date/Time")?;
    let report_date = Some(record.parse_date("Report Date")?);

    let description = util::fold_spaces(record.get_value("Description")?);
    let description = description.as_ref();

    lazy_static! {
        static ref GENERIC_REGEX: Regex = Regex::new(&format!(concat!(
            r"^(?P<symbol>{symbol}) ?\({id}\) ",
            r"(?P<action>Spinoff|Split|Stock Dividend|Subscribable Rights Issue) ",
            r"(?:{id} )?(?P<to>[1-9]\d*) for (?P<from>[1-9]\d*) ",

            // The secondary symbol and its name may be suffixed with the following abbreviations:
            // * RT, RTS - rights (subscribable rights)
            // * WI, W/I - when issued
            //
            // See the examples in tests below.
            r"\((?P<other_symbol>{symbol})(?:{old_suffix})?(?: (?:RT|WI))*, [^,)]+, {id}\)$"),

            symbol=common::STOCK_SYMBOL_REGEX, old_suffix=regex::escape(common::OLD_SYMBOL_SUFFIX),
            id=SecurityID::REGEX)).unwrap();

        static ref LIQUIDATION_REGEX: Regex = Regex::new(&format!(concat!(
            r"^(?P<symbol>{symbol}) ?\({id}\) ",
            r"(?P<action>Merged\(Liquidation\)) ",
            r"FOR (?P<currency>[A-Z]{{3}}) (?P<price>[0-9.]+) PER SHARE ",
            r"\((?P<other_symbol>{symbol}), [^,)]+, {id}\)$"),
            symbol=common::STOCK_SYMBOL_REGEX, id=SecurityID::REGEX)).unwrap();
    }

    let captures: Captures = GENERIC_REGEX.captures(description)
        .or_else(|| LIQUIDATION_REGEX.captures(description))
        .ok_or_else(|| format!("Unsupported corporate action: {:?}", description))?;

    let symbol = parse_symbol(captures.name("symbol").unwrap().as_str())?;
    let other_symbol = parse_symbol(captures.name("other_symbol").unwrap().as_str())?;
    let error = || Err!("Unsupported corporate action: {:?}", description);

    let action = match captures.name("action").unwrap().as_str() {
        "Merged(Liquidation)" => {
            if other_symbol != symbol {
                return error();
            }

            let currency = record.get_value("Currency")?;
            let volume = record.parse_amount("Proceeds", DecimalRestrictions::PositiveOrZero)?;
            let quantity = -record.parse_quantity("Quantity", DecimalRestrictions::StrictlyNegative)?;

            let price = util::parse_decimal(
                captures.name("price").unwrap().as_str(),
                DecimalRestrictions::PositiveOrZero)?;

            let price_currency = captures.name("currency").unwrap().as_str();
            if price_currency != currency {
                return Err!("Price and volume currency mismatch: {} vs {}", price_currency, currency);
            }

            check_volume(quantity, Cash::new(currency, price), Cash::new(currency, volume))?;

            CorporateActionType::Liquidation {
                quantity, price, volume,
                currency: currency.to_owned(),
            }
        },

        "Spinoff" => {
            let quantity = record.parse_quantity("Quantity", DecimalRestrictions::StrictlyPositive)?;
            let currency = record.get_value("Currency")?.to_owned();

            CorporateActionType::Spinoff {
                symbol: other_symbol,
                quantity, currency,
            }
        },

        "Split" => {
            if other_symbol != symbol {
                return error();
            }

            let from: u32 = captures.name("from").unwrap().as_str().parse()?;
            let to: u32 = captures.name("to").unwrap().as_str().parse()?;
            let ratio = StockSplitRatio::new(from, to);

            let change = record.parse_quantity("Quantity", DecimalRestrictions::NonZero)?;
            let (withdrawal, deposit) = if change.is_sign_positive() {
                (None, Some(change))
            } else {
                (Some(-change), None)
            };

            CorporateActionType::StockSplit{ratio, withdrawal, deposit}
        },

        "Stock Dividend" => {
            let quantity = record.parse_quantity("Quantity", DecimalRestrictions::StrictlyPositive)?;
            CorporateActionType::StockDividend {
                stock: Some(other_symbol),
                quantity,
            }
        },

        "Subscribable Rights Issue" => CorporateActionType::SubscribableRightsIssue,

        _ => unreachable!(),
    };

    Ok(CorporateAction {time: time.into(), report_date, symbol, action})
}

fn join_stock_splits(mut actions: Vec<CorporateAction>) -> GenericResult<CorporateAction> {
    match actions.len() {
        0 => unreachable!(),
        1 => {
            // Simple stock split
            return Ok(actions.pop().unwrap())
        },
        2 => {
            // Complex stock splits are represented by two records
        },
        _ => {
            let action = actions.first().unwrap();
            return Err!(
                "Unsupported stock split: {} at {}",
                action.symbol, format_date(action.time.date));
        },
    };

    let supplementary_action = actions.pop().unwrap();
    let mut action = actions.pop().unwrap();

    let (ratio, withdrawal, deposit) = match (action.action, supplementary_action.action) {
        // It looks like the records may have an arbitrary order
        (
            CorporateActionType::StockSplit {ratio: first_ratio, withdrawal: Some(withdrawal), deposit: None},
            CorporateActionType::StockSplit {ratio: second_ratio, withdrawal: None, deposit: Some(deposit)},
        ) if first_ratio == second_ratio => {
            (first_ratio, withdrawal, deposit)
        },
        (
            CorporateActionType::StockSplit {ratio: first_ratio, withdrawal: None, deposit: Some(deposit)},
            CorporateActionType::StockSplit {ratio: second_ratio, withdrawal: Some(withdrawal), deposit: None},
        ) if first_ratio == second_ratio => {
            (first_ratio, withdrawal, deposit)
        },
        _ => {
            return Err!(
                "Unsupported stock split: {} at {}",
                action.symbol, format_date(action.time.date));
        },
    };

    action.action = CorporateActionType::StockSplit {
        ratio,
        withdrawal: Some(withdrawal),
        deposit: Some(deposit),
    };
    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn liquidation() {
        test_parsing(&[
            "Stocks", "USD", "2021-11-03", "2021-11-02, 20:25:00",
            "CHL(US16941M1099) Merged(Liquidation) FOR USD 30.20446 PER SHARE (CHL, CHINA MOBILE LTD-SPON ADR, US16941M1099)",
            "-10", "302.0446", "-275.1", "3.012343", "",
        ], CorporateAction {
            time: date_time!(2021, 11, 2, 20, 25, 00).into(),
            report_date: Some(date!(2021, 11, 3)),

            symbol: s!("CHL"),
            action: CorporateActionType::Liquidation {
                quantity: dec!(10),
                price: dec!(30.20446),
                volume: dec!(302.0446),
                currency: s!("USD"),
            },
        });
    }

    #[test]
    fn spinoff() {
        test_parsing(&[
            "Stocks", "USD", "2020-11-17", "2020-11-16, 20:25:00",
            "PFE(US7170811035) Spinoff  124079 for 1000000 (VTRS, VIATRIS INC-W/I, US92556V1061)",
            "9.3059", "0", "0", "0", "",
        ], CorporateAction {
            time: date_time!(2020, 11, 16, 20, 25, 00).into(),
            report_date: Some(date!(2020, 11, 17)),

            symbol: s!("PFE"),
            action: CorporateActionType::Spinoff {
                symbol: s!("VTRS"),
                quantity: dec!(9.3059),
                currency: s!("USD"),
            },
        });
    }

    #[test]
    fn stock_dividend() {
        test_parsing(&[
            "Stocks", "USD", "2020-07-17", "2020-07-17, 20:20:00",
            "TEF (US8793822086) Stock Dividend US8793822086 416666667 for 10000000000 (TEF, TELEFONICA SA-SPON ADR, US8793822086)",
            "1", "0", "4.73", "0", "",
        ], CorporateAction {
            time: date_time!(2020, 7, 17, 20, 20, 0).into(),
            report_date: Some(date!(2020, 7, 17)),

            symbol: s!("TEF"),
            action: CorporateActionType::StockDividend {
                stock: Some(s!("TEF")),
                quantity: dec!(1),
            },
        });
    }

    #[rstest(record, symbol, time, report_date, to, from, withdrawal, deposit,
        case(&[
            "Stocks", "USD", "2020-08-31", "2020-08-28, 20:25:00",
            "AAPL(US0378331005) Split 4 for 1 (AAPL, APPLE INC, US0378331005)",
            "111", "0", "0", "0", "",
        ], "AAPL", date_time!(2020, 8, 28, 20, 25, 00), date!(2020, 8, 31), 4, 1, None, Some(dec!(111))),

        case(&[
            "Stocks", "USD", "2021-01-21", "2021-01-20, 20:25:00",
            "SLG(US78440X1019) Split 100000 for 102918 (SLG.OLD, SL GREEN REALTY CORP, US78440X1019)",
            "-7", "0", "0", "0", "",
        ], "SLG", date_time!(2021, 1, 20, 20, 25, 00), date!(2021, 1, 21), 100000, 102918, Some(dec!(7)), None),

        case(&[
            "Stocks", "USD", "2020-08-03", "2020-07-31, 20:25:00",
            "VISL(US92836Y2019) Split 1 for 6 (VISL, VISLINK TECHNOLOGIES INC, US92836Y2019)",
            "-80", "0", "0", "0", "",
        ], "VISL", date_time!(2020, 7, 31, 20, 25, 00), date!(2020, 8, 3), 1, 6, Some(dec!(80)), None),
        case(&[
            "Stocks", "USD", "2020-08-03", "2020-07-31, 20:25:00",
            "VISL(US92836Y2019) Split 1 for 6 (VISL, VISLINK TECHNOLOGIES INC, US92836Y3009)",
            "13.3333", "0", "0", "0", "",
        ], "VISL", date_time!(2020, 7, 31, 20, 25, 00), date!(2020, 8, 3), 1, 6, None, Some(dec!(13.3333))),
    )]
    fn stock_split(
        record: &[&str], symbol: &str, time: DateTime, report_date: Date, to: u32, from: u32,
        withdrawal: Option<Decimal>, deposit: Option<Decimal>,
    ) {
        test_parsing(record, CorporateAction {
            time: time.into(),
            report_date: Some(report_date),

            symbol: symbol.to_owned(),
            action: CorporateActionType::StockSplit{
                ratio: StockSplitRatio::new(from, to),
                withdrawal, deposit,
            },
        });
    }

    #[rstest(record, symbol, time, report_date,
        case(&[
            "Stocks", "USD", "2021-06-18", "2021-06-16, 20:25:00",
            "BST(US09258G1040) Subscribable Rights Issue  1 for 1 (BST RT WI, BLACKROCK SCIENCE -RTS W/I, US09258G1123)",
            "6", "0", "0", "0", "",
        ], "BST", date_time!(2021, 6, 16, 20, 25, 00), date!(2021, 6, 18)),

        case(&[
            "Stocks", "HKD", "2021-08-12", "2021-08-11, 20:25:00",
            "698(KYG8917X1218) Subscribable Rights Issue  1 for 2 (2965, TONGDA GROUP HOLDINGS LTD - RIGHTS, KYG8917X1RTS)",
            "25000", "0", "0", "0",
        ], "698", date_time!(2021, 8, 11, 20, 25, 00), date!(2021, 8, 12)),
    )]
    fn subscribable_rights_issue(record: &[&str], symbol: &str, time: DateTime, report_date: Date) {
        test_parsing(record, CorporateAction {
            time: time.into(),
            report_date: Some(report_date),

            symbol: symbol.to_owned(),
            action: CorporateActionType::SubscribableRightsIssue,
        });
    }

    fn test_parsing(record: &[&str], expected: CorporateAction) {
        let fields =
            "Asset Category,Currency,Report Date,Date/Time,Description,Quantity,Proceeds,Value,Realized P/L,Code"
            .split(',').collect();
        let spec = RecordSpec::new("test", fields, 0);

        let record = StringRecord::from(record);
        let record = Record::new(&spec, &record);
        assert_eq!(parse(&record).unwrap(), expected);
    }
}