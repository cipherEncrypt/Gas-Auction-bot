use crate::error::AnalysisError;
use crate::types::Gwei;
use ethers::types::{
    transaction::eip2718::TypedTransaction, Address, Bytes, NameOrAddress, Transaction, TxHash,
    U256,
};
use std::fmt;

/// Known contract interaction decoded from transaction calldata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractInteraction {
    Erc20Transfer {
        token: Address,
        recipient: Address,
        amount: U256,
    },
    Erc20TransferFrom {
        token: Address,
        owner: Address,
        recipient: Address,
        amount: U256,
    },
    Erc721TransferFrom {
        contract: Address,
        from: Address,
        to: Address,
        token_id: U256,
    },
    ContractCall {
        selector: [u8; 4],
        signature: Option<&'static str>,
    },
}

/// Whether the transaction uses legacy gas pricing or EIP-1559.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionFeeModel {
    Legacy,
    Eip1559,
}

/// Normalized view of a pending transaction ready for profitability analysis.
#[derive(Debug, Clone)]
pub struct ParsedTransaction {
    pub hash: TxHash,
    pub from: Address,
    pub to: Option<Address>,
    pub value: U256,
    pub gas_limit: U256,
    pub gas_price: Option<U256>,
    pub max_fee_per_gas: Option<U256>,
    pub max_priority_fee_per_gas: Option<U256>,
    pub input: Bytes,
    pub nonce: U256,
    pub fee_model: TransactionFeeModel,
    pub interaction: Option<ContractInteraction>,
}

impl ParsedTransaction {
    /// Effective gas price used for filtering and cost estimation.
    pub fn effective_gas_price(&self) -> U256 {
        match self.fee_model {
            TransactionFeeModel::Legacy => self.gas_price.unwrap_or_default(),
            TransactionFeeModel::Eip1559 => self.max_fee_per_gas.unwrap_or_default(),
        }
    }

    pub fn effective_gas_price_gwei(&self) -> f64 {
        wei_to_gwei(self.effective_gas_price())
    }

    pub fn value_eth(&self) -> f64 {
        wei_to_eth(self.value)
    }
}

/// Parses a raw RPC transaction into a structured, analysis-ready representation.
pub fn parse_transaction(transaction: Transaction) -> Result<ParsedTransaction, AnalysisError> {
    let hash = transaction.hash;
    let from = transaction.from;

    let gas_limit = transaction.gas;
    let input = transaction.input.clone();
    let interaction = decode_contract_interaction(transaction.to, &input);

    let (fee_model, gas_price, max_fee_per_gas, max_priority_fee_per_gas) =
        match transaction.transaction_type {
            Some(type_id) if type_id.as_u64() == 2 => (
                TransactionFeeModel::Eip1559,
                None,
                transaction.max_fee_per_gas,
                transaction.max_priority_fee_per_gas,
            ),
            _ => (
                TransactionFeeModel::Legacy,
                transaction.gas_price,
                None,
                None,
            ),
        };

    Ok(ParsedTransaction {
        hash,
        from,
        to: transaction.to,
        value: transaction.value,
        gas_limit,
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        input,
        nonce: transaction.nonce,
        fee_model,
        interaction,
    })
}

/// Parses a typed transaction before submission (used in replacement construction).
pub fn parse_typed_transaction(
    typed: &TypedTransaction,
    from: Address,
) -> Result<ParsedTransaction, AnalysisError> {
    let hash = typed.sighash();
    let to = resolve_transaction_target(typed.to());
    let input = typed.data().cloned().unwrap_or_default();
    let interaction = decode_contract_interaction(to, &input);

    let (fee_model, gas_price, max_fee_per_gas, max_priority_fee_per_gas) = match typed {
        TypedTransaction::Legacy(inner) => {
            (TransactionFeeModel::Legacy, inner.gas_price, None, None)
        }
        TypedTransaction::Eip2930(inner) => {
            (TransactionFeeModel::Legacy, inner.tx.gas_price, None, None)
        }
        TypedTransaction::Eip1559(inner) => (
            TransactionFeeModel::Eip1559,
            None,
            inner.max_fee_per_gas,
            inner.max_priority_fee_per_gas,
        ),
    };

    Ok(ParsedTransaction {
        hash,
        from,
        to,
        value: typed.value().copied().unwrap_or_default(),
        gas_limit: typed.gas().copied().unwrap_or_default(),
        gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        input,
        nonce: typed.nonce().copied().unwrap_or_default(),
        fee_model,
        interaction,
    })
}

fn resolve_transaction_target(target: Option<&NameOrAddress>) -> Option<Address> {
    match target {
        Some(NameOrAddress::Address(address)) => Some(*address),
        Some(NameOrAddress::Name(_)) | None => None,
    }
}

fn decode_contract_interaction(
    contract: Option<Address>,
    input: &Bytes,
) -> Option<ContractInteraction> {
    if input.len() < 4 {
        return None;
    }

    let contract = contract?;
    let mut selector = [0u8; 4];
    selector.copy_from_slice(&input[..4]);

    match selector {
        ERC20_TRANSFER => decode_erc20_transfer(contract, input),
        ERC20_TRANSFER_FROM => decode_erc20_transfer_from(contract, input),
        ERC721_SAFE_TRANSFER_FROM => decode_erc721_transfer_from(contract, input),
        other => Some(ContractInteraction::ContractCall {
            selector: other,
            signature: lookup_known_selector(&other),
        }),
    }
}

fn decode_erc20_transfer(contract: Address, input: &Bytes) -> Option<ContractInteraction> {
    if input.len() < 68 {
        return None;
    }

    let recipient = address_from_word(&input[16..36]);
    let amount = U256::from_big_endian(&input[36..68]);

    Some(ContractInteraction::Erc20Transfer {
        token: contract,
        recipient,
        amount,
    })
}

fn decode_erc20_transfer_from(contract: Address, input: &Bytes) -> Option<ContractInteraction> {
    if input.len() < 100 {
        return None;
    }

    let owner = address_from_word(&input[16..36]);
    let recipient = address_from_word(&input[48..68]);
    let amount = U256::from_big_endian(&input[68..100]);

    Some(ContractInteraction::Erc20TransferFrom {
        token: contract,
        owner,
        recipient,
        amount,
    })
}

fn decode_erc721_transfer_from(contract: Address, input: &Bytes) -> Option<ContractInteraction> {
    if input.len() < 100 {
        return None;
    }

    let from = address_from_word(&input[16..36]);
    let to = address_from_word(&input[48..68]);
    let token_id = U256::from_big_endian(&input[68..100]);

    Some(ContractInteraction::Erc721TransferFrom {
        contract,
        from,
        to,
        token_id,
    })
}

fn address_from_word(word: &[u8]) -> Address {
    Address::from_slice(&word[word.len().saturating_sub(20)..])
}

fn lookup_known_selector(selector: &[u8; 4]) -> Option<&'static str> {
    match *selector {
        UNISWAP_V2_SWAP => {
            Some("swapExactTokensForTokens(uint256,uint256,address[],address,uint256)")
        }
        UNISWAP_V3_EXACT_INPUT => Some("exactInput((bytes,address,uint256,uint256,uint256))"),
        _ => None,
    }
}

pub fn wei_to_gwei(wei: U256) -> f64 {
    wei.as_u128() as f64 / 1_000_000_000.0
}

pub fn gwei_to_wei(gwei: Gwei) -> U256 {
    U256::from(gwei.to_wei())
}

pub fn wei_to_eth(wei: U256) -> f64 {
    let base = U256::exp10(18);
    let whole = wei / base;
    let remainder = wei % base;
    whole.as_u128() as f64 + remainder.as_u128() as f64 / 1e18
}

impl fmt::Display for ParsedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "tx {} from {} value {} gas {}",
            self.hash,
            self.from,
            self.value_eth(),
            self.effective_gas_price_gwei()
        )
    }
}

const ERC20_TRANSFER: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];
const ERC20_TRANSFER_FROM: [u8; 4] = [0x23, 0xb8, 0x72, 0xdd];
const ERC721_SAFE_TRANSFER_FROM: [u8; 4] = [0x42, 0x84, 0x2e, 0x0e];
const UNISWAP_V2_SWAP: [u8; 4] = [0x38, 0xed, 0x17, 0x39];
const UNISWAP_V3_EXACT_INPUT: [u8; 4] = [0xc0, 0x4b, 0x8d, 0x59];

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::{Transaction, H256, U64};

    fn sample_legacy_tx() -> Transaction {
        Transaction {
            hash: H256::repeat_byte(0xab),
            nonce: U256::from(7),
            block_hash: None,
            block_number: None,
            transaction_index: None,
            from: Address::repeat_byte(0x01),
            to: Some(Address::repeat_byte(0x02)),
            value: U256::exp10(17),
            gas_price: Some(U256::from(30_000_000_000u64)),
            gas: U256::from(21000),
            input: Bytes::new(),
            v: U64::from(27),
            r: U256::from(1),
            s: U256::from(1),
            transaction_type: Some(U64::from(0)),
            access_list: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            chain_id: Some(U256::from(1)),
            other: Default::default(),
        }
    }

    #[test]
    fn parses_legacy_transaction_fields() {
        let parsed = parse_transaction(sample_legacy_tx()).expect("parse succeeds");
        assert_eq!(parsed.fee_model, TransactionFeeModel::Legacy);
        assert_eq!(parsed.effective_gas_price(), U256::from(30_000_000_000u64));
        assert!((parsed.value_eth() - 0.1).abs() < 1e-12);
    }

    #[test]
    fn parses_eip1559_transaction() {
        let mut tx = sample_legacy_tx();
        tx.transaction_type = Some(U64::from(2));
        tx.gas_price = None;
        tx.max_fee_per_gas = Some(U256::from(50_000_000_000u64));
        tx.max_priority_fee_per_gas = Some(U256::from(2_000_000_000u64));

        let parsed = parse_transaction(tx).expect("parse succeeds");
        assert_eq!(parsed.fee_model, TransactionFeeModel::Eip1559);
        assert_eq!(parsed.effective_gas_price(), U256::from(50_000_000_000u64));
    }

    #[test]
    fn decodes_erc20_transfer_calldata() {
        let token = Address::repeat_byte(0xaa);
        let recipient = Address::repeat_byte(0xbb);
        let amount = U256::from(1_000_000u64);

        let mut input = Vec::with_capacity(68);
        input.extend_from_slice(&ERC20_TRANSFER);
        input.extend_from_slice(&[0u8; 12]);
        input.extend_from_slice(recipient.as_bytes());
        let mut amount_bytes = [0u8; 32];
        amount.to_big_endian(&mut amount_bytes);
        input.extend_from_slice(&amount_bytes);

        let mut tx = sample_legacy_tx();
        tx.to = Some(token);
        tx.input = Bytes::from(input);

        let parsed = parse_transaction(tx).expect("parse succeeds");
        match parsed.interaction {
            Some(ContractInteraction::Erc20Transfer {
                token: parsed_token,
                recipient: parsed_recipient,
                amount: parsed_amount,
            }) => {
                assert_eq!(parsed_token, token);
                assert_eq!(parsed_recipient, recipient);
                assert_eq!(parsed_amount, amount);
            }
            other => panic!("expected ERC20 transfer, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_input_for_contract_call() {
        let parsed = parse_transaction(sample_legacy_tx()).expect("parse succeeds");
        assert!(parsed.interaction.is_none());
    }
}
