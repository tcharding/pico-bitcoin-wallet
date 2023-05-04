use anyhow::{Result, Context, anyhow, bail};
use std::convert::TryInto;
use bitcoin::{Transaction, TxIn, TxOut, OutPoint, Amount, Address, PrivateKey, Sequence, Network, transaction, FeeRate, Witness};

mod config;
mod db;

fn load_private_key() -> Result<PrivateKey> {
    let data_dir = dirs::data_dir().ok_or(anyhow!("The user data directory was not identified"))?.join("pico-bitcoin-wallet");
    std::fs::create_dir_all(&data_dir).with_context(|| format!("Failed to create data dir at {}", data_dir.display()))?;
    let path = data_dir.join("private.key");
    match std::fs::read_to_string(&path) {
        Ok(key) => key.parse().context("Failed to parse private key"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = PrivateKey::new(secp256k1::SecretKey::new(&mut rand::thread_rng()), Network::Regtest);
            std::fs::write(&path, key.to_wif().as_bytes()).context("failed to save private key")?;
            Ok(key)
        },
        Err(error) => Err(anyhow!(error).context("failed to read private key")),
    }
}

fn scan() -> Result<()> {
    use bitcoincore_rpc::RpcApi;

    let conf = config::load()?;
    let connection = bitcoincore_rpc::Client::new(&conf.bitcoind_uri, conf.bitcoind_auth).context("failed to connect to bitcoind")?;
    let current_height = connection.get_block_count().context("Failed to get block count")?;
    let mut db = db::Db::open()?;
    let last_height = db.get_last_height()?;
    let script_pubkey = get_address()?.script_pubkey();
    // we need to move txid below but not `script_pubkey`
    let script_pubkey = &script_pubkey;
    let mut block_count = 0;
    let mut tx_count = 0;
    let mut txos = 0;
    let mut total_amount = 0;
    let txos_iter = ((last_height + 1)..=current_height).flat_map(|height| {
        let block = connection.get_block_hash(height).context("Failed to get block hash")
            .and_then(|block_hash| {
                connection.get_block(&block_hash).context("Failed to get block hash")
            });
        match block {
            Ok(block) => {
                block_count += 1;
                either::Left(block.txdata.into_iter().map(Ok))
            },
            Err(error) => either::Right(std::iter::once(Err(error))),
        }
    })
    .flat_map(|transaction| {
        match transaction {
            Ok(transaction) => {
                tx_count += 1;
                let txid = transaction.txid();
                let iter = transaction
                    .output
                    .into_iter()
                    .enumerate()
                    .map(move |(i, txout)| Ok((txid, i, txout)));
                either::Left(iter)
            },
            Err(error) => either::Right(std::iter::once(Err(error))),
        }
    })
    .filter_map(|result| {
        match result {
            Ok((txid, i, txout)) => {
                if txout.script_pubkey == *script_pubkey {
                    txos += 1;
                    total_amount += txout.value;
                    let out_point = OutPoint { txid, vout: i.try_into().unwrap() };
                    Some(Ok((out_point, txout.value)))
                } else {
                    None
                }
            },
            Err(error) => Some(Err(error)),
        }
    });
    db.store_txos(txos_iter, current_height)?;
    println!("Scanned {} blocks and {} transactions, found {} txos totalling {} sats.", block_count, tx_count, txos, total_amount);
    Ok(())
}

fn get_address() -> Result<Address> {
    let private_key = load_private_key()?;
    let pub_key = private_key.inner.x_only_public_key(&**secp256k1::SECP256K1).0;
    Ok(Address::p2tr(&secp256k1::SECP256K1, pub_key, None, Network::Regtest))
}

fn address() -> Result<()> {
    println!("{}", get_address()?);
    Ok(())
}

fn send(mut args: std::env::Args) -> Result<()> {
    use bitcoincore_rpc::RpcApi;
    use bitcoin::key::TapTweak;

    let conf = config::load()?;
    let mut db = db::Db::open()?;
    let connection = bitcoincore_rpc::Client::new(&conf.bitcoind_uri, conf.bitcoind_auth).context("failed to connect to bitcoind")?;
    let address = args.next().ok_or_else(|| anyhow!("Missing address"))?
        .parse::<Address<_>>()
        .context("Invalid bitcoin address")?
        .require_network(Network::Regtest)
        .context("Invalid bitcoin address")?;
    let amount = args.next().ok_or_else(|| anyhow!("Missing amount"))?
        .parse::<Amount>()
        .context("Invalid amount")?;

    let payee_script_pubkey = address.script_pubkey();
    let private_key = load_private_key()?;
    let key_pair = secp256k1::KeyPair::from_secret_key(secp256k1::SECP256K1, &private_key.inner).tap_tweak(secp256k1::SECP256K1, None).to_inner();
    let script_pubkey = get_address()?.script_pubkey();
    let mut txins = Vec::new();
    let mut prevouts = Vec::new();
    for result in db.iter_unspent()?.iter()? {
        let (prev_out, amt) = result?;
        let txin = TxIn {
                        previous_output: prev_out,
                        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                        script_sig: Default::default(),
                        witness: Default::default(),
                    };
        txins.push(txin);
        let prevout = TxOut { script_pubkey: script_pubkey.clone(), value: amt.to_sat() };
        prevouts.push(prevout);
    }
    let total_amt = prevouts.iter()
        .map(|txout| Amount::from_sat(txout.value))
        .sum::<Amount>();
    let remaining = total_amt.checked_sub(amount).ok_or_else(|| anyhow!("Not enough money, you have {}", total_amt))?;
    let weight = transaction::predict_weight(txins.iter().map(|_| transaction::InputWeightPrediction::from_slice(0, &[64])), [payee_script_pubkey.len(), script_pubkey.len()].iter().copied());
    let fee = weight * FeeRate::BROADCAST_MIN;
    let change_amount = remaining.checked_sub(fee).ok_or_else(|| anyhow!("Not enough money, you have {}", total_amt))?;
    let payment = TxOut {
        script_pubkey: payee_script_pubkey,
        value: amount.to_sat(),
    };
    let change = TxOut {
        script_pubkey: script_pubkey.clone(),
        value: change_amount.to_sat(),
    };
    let mut transaction = Transaction {
        version: 2,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: txins,
        output: vec![payment, change],
    };
    let prevouts = bitcoin::psbt::Prevouts::All(&prevouts);
    let mut cache = bitcoin::sighash::SighashCache::new(&mut transaction);
    for i in 0..cache.transaction().input.len() {
        let hash = cache.taproot_key_spend_signature_hash(i, &prevouts, bitcoin::sighash::TapSighashType::Default).unwrap();
        let signature = secp256k1::SECP256K1.sign_schnorr(&hash.into(), &key_pair);
        *cache.witness_mut(i).unwrap() = Witness::from_slice(&[signature.as_ref()]);
    }
    connection.send_raw_transaction(&transaction).context("failed to broadcast transaction")?;
    for input in transaction.input {
        db.set_spent(&input.previous_output)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args();
    args.next().ok_or_else(|| anyhow!("not even program name given"))?;
    let command = args.next().ok_or_else(|| anyhow!("missing command"))?;
    match &*command {
        "scan" => scan(),
        "address" => address(),
        "send" => send(args),
        _ => bail!("Unknown command: `{}`", command),
    }
}
