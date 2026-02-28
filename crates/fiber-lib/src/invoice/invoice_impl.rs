use fiber_types::{
    Attribute, CkbInvoice, CkbScript, Currency, FeatureVector, Hash256, HashAlgorithm, InvoiceData,
    InvoiceError, MAX_DESCRIPTION_LENGTH,
};

use crate::time::UNIX_EPOCH;
use secp256k1::{ecdsa::RecoverableSignature, Message, PublicKey};
use std::time::Duration;

pub struct InvoiceBuilder {
    currency: Currency,
    amount: Option<u128>,
    payment_hash: Option<Hash256>,
    payment_preimage: Option<Hash256>,
    attrs: Vec<Attribute>,
}

impl Default for InvoiceBuilder {
    fn default() -> Self {
        Self::new(Currency::Fibb)
    }
}

macro_rules! attr_setter {
    ($name:ident, $attr:ident, $param:ty) => {
        pub fn $name(self, value: $param) -> Self {
            self.add_attr(Attribute::$attr(value))
        }
    };
}

impl InvoiceBuilder {
    pub fn new(currency: Currency) -> Self {
        Self {
            currency,
            amount: None,
            payment_hash: None,
            payment_preimage: None,
            attrs: Vec::new(),
        }
    }

    pub fn currency(mut self, currency: Currency) -> Self {
        self.currency = currency;
        self
    }

    pub fn amount(mut self, amount: Option<u128>) -> Self {
        self.amount = amount;
        self
    }

    pub fn add_attr(mut self, attr: Attribute) -> Self {
        self.attrs.push(attr);
        self
    }

    pub fn update_attr(mut self, attr: Attribute) -> Self {
        let mut found = false;
        for existing_attr in self.attrs.iter_mut() {
            if std::mem::discriminant(existing_attr) == std::mem::discriminant(&attr) {
                *existing_attr = attr.clone();
                found = true;
                break;
            }
        }
        if !found {
            self.attrs.push(attr);
        }
        self
    }

    /// The hash of the preimage. If hash is set, preimage must be absent.
    /// This condition indicates a 'hold invoice' for which the tlc must be
    /// accepted and held until the preimage becomes known.
    pub fn payment_hash(mut self, payment_hash: Hash256) -> Self {
        self.payment_hash = Some(payment_hash);
        self
    }

    /// The preimage to settle an incoming TLC payable to this invoice.
    /// If preimage is set, hash must be absent. If both preimage and hash
    /// are absent, a random preimage should be generated and passed into
    /// the invoice builder, otherwise NeitherPaymenthashNorPreimage error
    /// is thrown.
    pub fn payment_preimage(mut self, payment_preimage: Hash256) -> Self {
        self.payment_preimage = Some(payment_preimage);
        self
    }

    pub fn udt_type_script(self, script: ckb_types::packed::Script) -> Self {
        self.add_attr(Attribute::UdtScript(CkbScript(script)))
    }

    attr_setter!(description, Description, String);
    attr_setter!(payee_pub_key, PayeePublicKey, PublicKey);
    attr_setter!(expiry_time, ExpiryTime, Duration);
    attr_setter!(fallback_address, FallbackAddr, String);
    attr_setter!(final_expiry_delta, FinalHtlcMinimumExpiryDelta, u64);
    attr_setter!(payment_secret, PaymentSecret, Hash256);
    attr_setter!(hash_algorithm, HashAlgorithm, HashAlgorithm);

    fn update_feature_vector<F>(self, f: F) -> Self
    where
        F: FnOnce(&mut FeatureVector),
    {
        let mut feature_vector = self
            .attrs
            .iter()
            .find_map(|attr| {
                if let Attribute::Feature(feature) = attr {
                    Some(feature.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(FeatureVector::new);

        f(&mut feature_vector);
        self.update_attr(Attribute::Feature(feature_vector))
    }

    pub fn allow_mpp(self, allow_mpp: bool) -> Self {
        self.update_feature_vector(|feature_vector| {
            if allow_mpp {
                feature_vector.set_basic_mpp_optional();
            } else {
                feature_vector.unset_basic_mpp_optional();
            }
        })
    }

    pub fn allow_trampoline_routing(self, allow: bool) -> Self {
        self.update_feature_vector(|feature_vector| {
            if allow {
                feature_vector.set_trampoline_routing_optional();
            } else {
                feature_vector.unset_trampoline_routing_optional();
            }
        })
    }

    pub fn build(self) -> Result<CkbInvoice, InvoiceError> {
        let payment_hash: Hash256 = match (self.payment_hash, self.payment_preimage) {
            (Some(payment_hash), None) => payment_hash,
            (None, Some(preimage)) => {
                let algo = self
                    .attrs
                    .iter()
                    .find_map(|attr| match attr {
                        Attribute::HashAlgorithm(algo) => Some(algo),
                        _ => None,
                    })
                    .copied()
                    .unwrap_or_default();
                algo.hash(preimage.as_ref()).into()
            }
            (Some(_), Some(_)) => return Err(InvoiceError::BothPaymenthashAndPreimage),
            (None, None) => return Err(InvoiceError::NeitherPaymenthashNorPreimage),
        };

        self.check_attrs_valid()?;
        let timestamp = UNIX_EPOCH
            .elapsed()
            .expect("Duration since unix epoch")
            .as_millis();
        Ok(CkbInvoice {
            currency: self.currency,
            amount: self.amount,
            signature: None,
            data: InvoiceData {
                timestamp,
                payment_hash,
                attrs: self.attrs,
            },
        })
    }

    pub fn build_with_sign<F>(self, sign_function: F) -> Result<CkbInvoice, InvoiceError>
    where
        F: FnOnce(&Message) -> RecoverableSignature,
    {
        let mut invoice = self.build()?;
        invoice.update_signature(sign_function)?;
        Ok(invoice)
    }

    fn check_attrs_valid(&self) -> Result<(), InvoiceError> {
        let allow_mpp = self.attrs.iter().any(
            |attr| matches!(attr, Attribute::Feature(feature) if feature.supports_basic_mpp()),
        );
        let payment_secret = self.attrs.iter().find_map(|attr| match attr {
            Attribute::PaymentSecret(secret) => Some(secret),
            _ => None,
        });

        if allow_mpp && payment_secret.is_none() {
            return Err(InvoiceError::PaymentSecretRequiredForMpp);
        }
        // check is there any duplicate attribute key set
        for (i, attr) in self.attrs.iter().enumerate() {
            for other in self.attrs.iter().skip(i + 1) {
                if std::mem::discriminant(attr) == std::mem::discriminant(other) {
                    return Err(InvoiceError::DuplicatedAttributeKey(format!("{:?}", attr)));
                }
            }
        }

        if let Some(len) = self.attrs.iter().find_map(|attr| match attr {
            Attribute::Description(desc) if desc.len() > MAX_DESCRIPTION_LENGTH => Some(desc.len()),
            _ => None,
        }) {
            return Err(InvoiceError::DescriptionTooLong(len));
        }

        // check is there any deprecated attribute key set
        if self
            .attrs
            .iter()
            .any(|attr| matches!(attr, Attribute::FinalHtlcTimeout(..)))
        {
            return Err(InvoiceError::DeprecatedAttribute(
                "FinalHtlcTimeout".to_string(),
            ));
        }

        Ok(())
    }
}
