# Super Smart Contract

Super Smart Contracts are Smart Contracts enhanced by AI. They can interact with users, learn from them, and adapt to their needs. This repository provides a simple example of a Super Smart Contract using AI APIs (OpenAI or Google Gemini) to respond to queries.


## This repository provides:

1. An oracle supporting **multiple AI providers** (OpenAI GPT-4o or Google Gemini 2.0)
2. A smart contract which serves as an interface to the oracle: LLMrieZMpbJFwN52WgmBNMxYojrpRVYXdC1RCweEbab
3. Two example of agents definitions:
   - A [simple agent](./programs/simple-agent) which queries the oracle and logs the response
   - An [agent which can dispense tokens](./programs/agent-minter) if convinced by the user knowledge of Solana
4. A [UI](./app) to interact with the agent minter


# How to create a Super Smart Contract

First, add the [solana-gpt-oracle](./programs/solana-gpt-oracle) as a dependency to your project. This program provides the interface to the OpenAI API.

```bash
cargo add solana-gpt-oracle
```

1. Define the Agent through a CPI into the LLM smart contract 

```rust
const AGENT_DESC: &str = "You are a helpful assistant.";

pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
  ctx.accounts.agent.context = ctx.accounts.llm_context.key();

  // Create the context for the AI agent
  let cpi_program = ctx.accounts.oracle_program.to_account_info();
  let cpi_accounts = solana_gpt_oracle::cpi::accounts::CreateLlmContext {
      payer: ctx.accounts.payer.to_account_info(),
      context_account: ctx.accounts.llm_context.to_account_info(),
      counter: ctx.accounts.counter.to_account_info(),
      system_program: ctx.accounts.system_program.to_account_info(),
  };
  let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
  solana_gpt_oracle::cpi::create_llm_context(cpi_ctx, AGENT_DESC.to_string())?;

  Ok(())
}
```

2. Create an instruction to interact with the agent, which specify the callback:

```rust
 pub fn interact_agent(ctx: Context<InteractAgent>, text: String) -> Result<()> {
   let cpi_program = ctx.accounts.oracle_program.to_account_info();
   let cpi_accounts = solana_gpt_oracle::cpi::accounts::InteractWithLlm {
      payer: ctx.accounts.payer.to_account_info(),
      interaction: ctx.accounts.interaction.to_account_info(),
      context_account: ctx.accounts.context_account.to_account_info(),
      system_program: ctx.accounts.system_program.to_account_info(),
   };
   let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
   solana_gpt_oracle::cpi::interact_with_llm(
      cpi_ctx,
      text,
      crate::ID,
      crate::instruction::CallbackFromAgent::discriminator(),
      None,
   )?;

   Ok(())
}
```

3. Define the callback to process the response:

```rust
pub fn callback_from_agent(ctx: Context<CallbackFromAgent>, response: String) -> Result<()> {
  // Check if the callback is from the LLM program
  if !ctx.accounts.identity.to_account_info().is_signer {
      return Err(ProgramError::InvalidAccountData.into());
  }
  // Do something with the response
  msg!("Agent Response: {:?}", response);
  Ok(())
}
```

The agent can be defined to create a textual response, a more complex json response or even an encoded instruction to be executed by the smart contract. See the [agent-minter](./programs/agent-minter) for an example of an agent that can dispense tokens.

### Building the programs

To build the programs, run:

```bash
anchor build
```

### Setting up the LLM Oracle Server

The LLM Oracle server is an off-chain service that monitors the blockchain for interaction requests and responds using AI APIs.

#### Prerequisites

1. **Choose your AI Provider** - You need an API key from either:
   - **Google Gemini** (recommended): Get your free API key at [https://aistudio.google.com/app/apikey](https://aistudio.google.com/app/apikey)
   - **OpenAI**: Get your API key at [https://platform.openai.com/api-keys](https://platform.openai.com/api-keys)

2. **Configure the Oracle**

Create a `.env` file in the `llm_oracle/` directory:

```bash
cd llm_oracle
cp .env.example .env  # If example doesn't exist, create .env manually
```

Edit `.env` with your API key:

```bash
# For Gemini (recommended - free tier available)
GEMINI_API_KEY=your-gemini-api-key-here

# OR for OpenAI
OPENAI_API_KEY=your-openai-api-key-here

# Solana RPC Configuration (optional, defaults to localhost)
RPC_URL=http://localhost:8899
WEBSOCKET_URL=ws://localhost:8900

# Oracle Identity (optional, uses test keypair by default)
# IDENTITY=your-base58-encoded-keypair-string
```

> **Note**: If both API keys are provided, Gemini takes priority. The oracle will automatically detect which key is available.

3. **Build the Oracle Server**

```bash
cd llm_oracle
cargo build --release
```

4. **Run the Oracle Server**

Start a local Solana validator (in a separate terminal):
```bash
solana-test-validator
```

Then run the oracle:
```bash
cd llm_oracle
cargo run --release
```

You should see output indicating which AI provider is being used:
```
🤖 Using Gemini AI (gemini-2.0-flash)
Oracle identity: tEsT3eV6RFCWs1BZ7AXTzasHqTtMnMLCB2tjQ42TDXD
RPC: "http://localhost:8899"
WS: "ws://localhost:8900"
```

#### Production Deployment

For production deployment:

1. **Generate a dedicated oracle keypair** (don't use the test key):
   ```bash
   solana-keygen new --outfile oracle-keypair.json
   ```

2. **Fund the oracle identity** with SOL for transaction fees:
   ```bash
   solana transfer <ORACLE_PUBKEY> 1 --allow-unfunded-recipient
   ```

3. **Set the IDENTITY environment variable**:
   ```bash
   export IDENTITY=$(solana-keygen pubkey oracle-keypair.json | base58)
   ```

4. **Deploy using Docker** (see `Dockerfile` and `fly.toml` for deployment examples)

### Running tests

To run the tests with the oracle server:

**Terminal 1** - Start the oracle server:
```bash
cd llm_oracle
cargo run --release
```

**Terminal 2** - Run the tests:
```bash
anchor test --skip-local-validator
```

To run tests without the oracle (basic functionality only):
```bash
anchor test
```
