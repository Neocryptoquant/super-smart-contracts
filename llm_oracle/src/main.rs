use anchor_lang::prelude::AccountMeta;
use anchor_lang::{AccountDeserialize, AnchorSerialize, Discriminator};
use chatgpt::client::ChatGPT;
use chatgpt::config::ModelConfiguration;
use chatgpt::types::{ChatMessage, Role};
use futures::StreamExt;
use memory::InteractionMemory;
use serde::{Deserialize, Serialize};
use solana_account_decoder::UiAccountEncoding;
use solana_client::pubsub_client::PubsubClient;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::env;
use std::error::Error;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

mod memory;

const MAX_TX_RETRY_ATTEMPTS: u8 = 5;
const MAX_API_RETRY_ATTEMPTS: u8 = 3;

// =============================================================================
// LLM Provider Abstraction (OpenAI + Gemini)
// =============================================================================

enum LLMProvider {
    OpenAI(ChatGPT),
    Gemini(GeminiClient),
}

impl LLMProvider {
    async fn send_message(&self, messages: &[ChatMessage]) -> Result<String, Box<dyn Error>> {
        match self {
            LLMProvider::OpenAI(client) => {
                let messages_vec = messages.to_vec();
                let response = client.send_history(&messages_vec).await?;
                Ok(response.message().content.clone())
            }
            LLMProvider::Gemini(client) => client.send_message(messages).await,
        }
    }
}

// Gemini API Client
struct GeminiClient {
    api_key: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
    role: String,
}

#[derive(Serialize)]
struct GeminiPart {
    text: String,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiResponseContent,
}

#[derive(Deserialize)]
struct GeminiResponseContent {
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    text: String,
}

impl GeminiClient {
    fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    async fn send_message(&self, messages: &[ChatMessage]) -> Result<String, Box<dyn Error>> {
        // 0xAbim: Added validation to prevent empty contents array
        if messages.is_empty() {
            return Err("Cannot send empty message history to Gemini API".into());
        }

        // Convert ChatMessage history to Gemini format
        let contents: Vec<GeminiContent> = messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::User => "user",
                    Role::System => "user", // Gemini doesn't have system role
                    Role::Assistant => "model",
                    Role::Function => "model", // Treat function as model
                };
                GeminiContent {
                    parts: vec![GeminiPart {
                        text: msg.content.clone(),
                    }],
                    role: role.to_string(),
                }
            })
            .collect();

        let request = GeminiRequest {
            contents,
            generation_config: GeminiGenerationConfig {
                temperature: 0.7,
                max_output_tokens: 100,
            },
        };

        // 0xAbim: Added Gemini API endpoint 
        let url = "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent";

        let response = self.client
            .post(url)
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await?;
            return Err(format!("Gemini API error ({}): {}", status, error_text).into());
        }

        let gemini_response: GeminiResponse = response.json().await?;

        if let Some(candidate) = gemini_response.candidates.first() {
            if let Some(part) = candidate.content.parts.first() {
                return Ok(part.text.clone());
            }
        }

        Err("No response from Gemini API".into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenv::dotenv().ok(); // Load .env file
    let (rpc_url, websocket_url, llm_provider, payer, identity_pda) = load_config()?;
    let mut interaction_memory = InteractionMemory::new(10);
    println!(" Oracle identity: {:?}", payer.pubkey());
    println!(" RPC: {:?}", rpc_url.as_str());
    println!(" WS: {:?}", websocket_url.as_str());
    loop {
        if let Err(e) = run_oracle(
            rpc_url.as_str(),
            websocket_url.as_str(),
            &llm_provider,
            &payer,
            &identity_pda,
            &mut interaction_memory,
        )
        .await
        {
            eprintln!("Error encountered: {:?}. Waiting 30 seconds before retry...", e);
            // 0xAbim: Added delay to prevent infinite loop on persistent errors
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        }
    }
}

async fn run_oracle(
    rpc_url: &str,
    websocket_url: &str,
    llm_provider: &LLMProvider,
    payer: &Keypair,
    identity_pda: &Pubkey,
    interaction_memory: &mut InteractionMemory,
) -> Result<(), Box<dyn Error>> {
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::processed());

    let (tx, rx) = mpsc::channel(100);
    let mut stream = ReceiverStream::new(rx);

    let rpc_config = RpcAccountInfoConfig {
        commitment: Some(CommitmentConfig::processed()),
        encoding: Some(UiAccountEncoding::Base64),
        ..Default::default()
    };

    let filters = vec![solana_client::rpc_filter::RpcFilterType::Memcmp(
        solana_client::rpc_filter::Memcmp::new(
            0,
            solana_client::rpc_filter::MemcmpEncodedBytes::Bytes(
                solana_gpt_oracle::Interaction::DISCRIMINATOR.to_vec(),
            ),
        ),
    )];

    fetch_and_process_program_accounts(
        &rpc_client,
        filters.clone(),
        payer,
        identity_pda,
        llm_provider,
        interaction_memory,
    )
    .await?;

    let program_config = RpcProgramAccountsConfig {
        account_config: rpc_config,
        filters: Some(filters),
        ..Default::default()
    };

    let subscription = PubsubClient::program_subscribe(
        &websocket_url,
        &solana_gpt_oracle::ID,
        Some(program_config),
    )?;

    tokio::spawn(async move {
        for update in subscription.1 {
            if tx.send(update).await.is_err() {
                eprintln!("Receiver dropped");
                break;
            }
        }
    });

    while let Some(update) = stream.next().await {
        if let Ok(interaction_pubkey) = Pubkey::from_str(&update.value.pubkey) {
            if let Some(data) = update.value.account.data.decode() {
                process_interaction(
                    payer,
                    identity_pda,
                    llm_provider,
                    &rpc_client,
                    interaction_pubkey,
                    data,
                    interaction_memory,
                )
                .await?;
            }
        }
    }

    Ok(())
}

/// Process an interaction and respond to it
async fn process_interaction(
    payer: &Keypair,
    identity_pda: &Pubkey,
    llm_provider: &LLMProvider,
    rpc_client: &RpcClient,
    interaction_pubkey: Pubkey,
    data: Vec<u8>,
    interaction_memory: &mut InteractionMemory,
) -> Result<(), Box<dyn Error>> {
    if let Ok(interaction) =
        solana_gpt_oracle::Interaction::try_deserialize_unchecked(&mut data.as_slice())
    {
        if interaction.is_processed == true {
            return Ok(());
        }
        println!("Processing interaction: {:?}", interaction_pubkey);
        if let Ok(context_data) = rpc_client.get_account(&interaction.context) {
            if let Ok(context) = solana_gpt_oracle::ContextAccount::try_deserialize_unchecked(
                &mut context_data.data.as_slice(),
            ) {
                println!(
                    "Interaction: {:?}, Pubkey: {:?}",
                    interaction, interaction_pubkey
                );

                // Get a response from the OpenAI API
                let mut previous_history = interaction_memory
                    .get_history(&interaction_pubkey)
                    .unwrap_or(Vec::new())
                    .clone();
                interaction_memory.add_interaction(
                    interaction_pubkey,
                    interaction.text.clone(),
                    Role::User,
                );
                previous_history.push(ChatMessage {
                    role: Role::User,
                    content: format!(
                        "With context: {:?}, respond to: {:?}",
                        context.text, interaction.text
                    ),
                });
                let mut api_attempts = 0;
                let mut response_content = String::new();
                while api_attempts < MAX_API_RETRY_ATTEMPTS {
                    match llm_provider.send_message(&previous_history).await {
                        Ok(response) => {
                            response_content = response;
                            break;
                        }
                        Err(e) => {
                            api_attempts += 1;
                            // 0xAbim: Improved retry logic - only skip messages if we have enough, keep at least 1
                            let skip_count = (api_attempts * 2) as usize;
                            if previous_history.len() > skip_count + 1 {
                                previous_history = previous_history
                                    .iter()
                                    .skip(skip_count)
                                    .cloned()
                                    .collect();
                            }
                            eprintln!(
                                "API call failed (attempt {}/{}): {:?}",
                                api_attempts, MAX_API_RETRY_ATTEMPTS, e
                            );
                            if api_attempts >= MAX_API_RETRY_ATTEMPTS {
                                return Err(e);
                            }
                        }
                    }
                }

                interaction_memory.add_interaction(
                    interaction_pubkey,
                    response_content.clone(),
                    Role::System,
                );

                let response_data = [
                    solana_gpt_oracle::instruction::CallbackFromLlm::DISCRIMINATOR.to_vec(),
                    response_content.try_to_vec()?,
                ]
                .concat();

                let mut callback_instruction = Instruction {
                    program_id: solana_gpt_oracle::ID,
                    accounts: vec![
                        AccountMeta::new(payer.pubkey(), true),
                        AccountMeta::new_readonly(*identity_pda, false),
                        AccountMeta::new(interaction_pubkey, false),
                        AccountMeta::new_readonly(interaction.callback_program_id, false),
                    ],
                    data: response_data,
                };

                // Add the remaining accounts from the callback_account_metas
                let remaining_accounts: Vec<AccountMeta> = interaction
                    .callback_account_metas
                    .iter()
                    .map(|meta| AccountMeta {
                        pubkey: meta.pubkey,
                        is_signer: meta.is_signer,
                        is_writable: meta.is_writable,
                    })
                    .collect();
                callback_instruction.accounts.extend(remaining_accounts);

                // Send the response with the callback transaction
                let mut attempts = 0;
                while attempts < MAX_TX_RETRY_ATTEMPTS {
                    if let Ok(recent_blockhash) = rpc_client
                        .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
                    {
                        let compute_budget_instruction =
                            ComputeBudgetInstruction::set_compute_unit_limit(300_000);
                        let priority_fee_instruction =
                            ComputeBudgetInstruction::set_compute_unit_price(1_000_000);

                        let transaction = Transaction::new_signed_with_payer(
                            &[
                                compute_budget_instruction,
                                priority_fee_instruction,
                                callback_instruction.clone(),
                            ],
                            Some(&payer.pubkey()),
                            &[&payer],
                            recent_blockhash.0,
                        );

                        match rpc_client.send_and_confirm_transaction(&transaction) {
                            Ok(signature) => {
                                println!("Transaction signature: {}\n", signature);
                                break;
                            }
                            Err(e) => {
                                attempts += 1;
                                eprintln!("Failed to send transaction: {:?}\n", e)
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Fetch all open interactions and process them
async fn fetch_and_process_program_accounts(
    rpc_client: &RpcClient,
    filters: Vec<solana_client::rpc_filter::RpcFilterType>,
    payer: &Keypair,
    identity_pda: &Pubkey,
    llm_provider: &LLMProvider,
    interaction_memory: &mut InteractionMemory,
) -> Result<(), Box<dyn Error>> {
    let rpc_config = RpcAccountInfoConfig {
        commitment: Some(CommitmentConfig::processed()),
        encoding: Some(UiAccountEncoding::Base64),
        ..Default::default()
    };

    let program_config = RpcProgramAccountsConfig {
        account_config: rpc_config,
        filters: Some(filters),
        ..Default::default()
    };

    let accounts =
        rpc_client.get_program_accounts_with_config(&solana_gpt_oracle::ID, program_config)?;

    for (pubkey, account) in accounts {
        process_interaction(
            payer,
            identity_pda,
            llm_provider,
            rpc_client,
            pubkey,
            account.data,
            interaction_memory,
        )
        .await?;
    }

    Ok(())
}

/// Load the Oracle configuration
fn load_config() -> Result<(String, String, LLMProvider, Keypair, Pubkey), Box<dyn Error>> {
    let identity = env::var("IDENTITY").unwrap_or(
        "62LxqpAW6SWhp7iKBjCQneapn1w6btAhW7xHeREWSpPzw3xZbHCfAFesSR4R76ejQXCLWrndn37cKCCLFvx6Swps"
            .to_string(),
    );
    let rpc_url = env::var("RPC_URL").unwrap_or("https://devnet.magicblock.app/".to_string());
    let websocket_url = env::var("WEBSOCKET_URL").unwrap_or("ws://devnet.magicblock.app/".to_string());

    // Detect which LLM provider to use based on API keys
    let llm_provider = if let Ok(gemini_key) = env::var("GEMINI_API_KEY") {
        if !gemini_key.is_empty() && gemini_key != "your-gemini-api-key-here" {
            println!("🤖 Using Gemini AI (gemini-2.0-flash)");
            LLMProvider::Gemini(GeminiClient::new(gemini_key))
        } else if let Ok(openai_key) = env::var("OPENAI_API_KEY") {
            if !openai_key.is_empty() {
                println!("🤖 Using OpenAI (gpt-4o)");
                LLMProvider::OpenAI(ChatGPT::new_with_config(
                    openai_key.as_str(),
                    ModelConfiguration {
                        engine: chatgpt::config::ChatGPTEngine::Custom("gpt-4o"),
                        presence_penalty: 0.3,
                        frequency_penalty: 0.3,
                        max_tokens: Some(100),
                        ..Default::default()
                    },
                )?)
            } else {
                return Err("No valid API key found. Please set GEMINI_API_KEY or OPENAI_API_KEY in .env file".into());
            }
        } else {
            return Err("No valid API key found. Please set GEMINI_API_KEY or OPENAI_API_KEY in .env file".into());
        }
    } else if let Ok(openai_key) = env::var("OPENAI_API_KEY") {
        if !openai_key.is_empty() {
            println!("🤖 Using OpenAI (gpt-4o)");
            LLMProvider::OpenAI(ChatGPT::new_with_config(
                openai_key.as_str(),
                ModelConfiguration {
                    engine: chatgpt::config::ChatGPTEngine::Custom("gpt-4o"),
                    presence_penalty: 0.3,
                    frequency_penalty: 0.3,
                    max_tokens: Some(100),
                    ..Default::default()
                },
            )?)
        } else {
            return Err("No valid API key found. Please set GEMINI_API_KEY or OPENAI_API_KEY in .env file".into());
        }
    } else {
        return Err("No valid API key found. Please set GEMINI_API_KEY or OPENAI_API_KEY in .env file".into());
    };

    let payer = Keypair::from_base58_string(&identity);
    let identity_pda = Pubkey::find_program_address(&[b"identity"], &solana_gpt_oracle::ID).0;
    Ok((rpc_url, websocket_url, llm_provider, payer, identity_pda))
}
