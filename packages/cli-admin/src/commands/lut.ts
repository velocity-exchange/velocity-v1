import { Command } from 'commander';
import {
	AddressLookupTableProgram,
	PublicKey,
	SystemProgram,
} from '@solana/web3.js';
import {
	getInsuranceFundVaultPublicKey,
	getPerpMarketPublicKeySync,
	getSpotMarketPublicKeySync,
	getSpotMarketVaultPublicKey,
	getVelocitySignerPublicKey,
	getVelocityStateAccountPublicKey,
	initialize,
} from '@velocity-exchange/sdk';
import { readGlobalOpts, withGlobalOptions } from '../lib/options';
import { buildAdminClient, buildProvider } from '../lib/provider';
import { reportDispatch, reportDryRun, sendOrPropose } from '../lib/squads';

const TOKEN_PROGRAM = new PublicKey(
	'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
);
const ASSOCIATED_TOKEN_PROGRAM = new PublicKey(
	'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'
);
/** Address lookup tables hold at most 256 entries. */
const LUT_CAPACITY = 256;
/** Addresses per extend instruction, keeping the transaction under the size limit. */
const EXTEND_CHUNK = 20;

type Entry = { label: string; address: PublicKey };

/**
 * Every account a fill, settle, or liquidate transaction references, derived
 * from the live market counts in `State` rather than a hardcoded list, so the
 * set cannot go stale when a market is added.
 */
async function collectMarketAddresses(client: any): Promise<Entry[]> {
	const programId = client.program.programId;
	const state = client.getStateAccount();
	const entries: Entry[] = [
		{
			label: 'state',
			address: await getVelocityStateAccountPublicKey(programId),
		},
		{
			label: 'velocity signer',
			address: getVelocitySignerPublicKey(programId),
		},
		{ label: 'token program', address: TOKEN_PROGRAM },
		{ label: 'associated token program', address: ASSOCIATED_TOKEN_PROGRAM },
		{ label: 'system program', address: SystemProgram.programId },
	];

	for (let i = 0; i < state.numberOfSpotMarkets; i++) {
		const market = client.getSpotMarketAccount(i);
		if (!market) {
			throw new Error(`spot market ${i} counted in State but not readable`);
		}
		const name = Buffer.from(market.name).toString('utf8').trim();
		entries.push(
			{
				label: `spot ${i} (${name})`,
				address: getSpotMarketPublicKeySync(programId, i),
			},
			{ label: `spot ${i} oracle`, address: market.oracle },
			{ label: `spot ${i} mint`, address: market.mint },
			{
				label: `spot ${i} vault`,
				address: await getSpotMarketVaultPublicKey(programId, i),
			},
			{
				label: `spot ${i} IF vault`,
				address: await getInsuranceFundVaultPublicKey(programId, i),
			}
		);
	}

	for (let i = 0; i < state.numberOfMarkets; i++) {
		const market = client.getPerpMarketAccount(i);
		if (!market) {
			throw new Error(`perp market ${i} counted in State but not readable`);
		}
		const name = Buffer.from(market.name).toString('utf8').trim();
		entries.push(
			{
				label: `perp ${i} (${name})`,
				address: getPerpMarketPublicKeySync(programId, i),
			},
			{ label: `perp ${i} oracle`, address: market.oracle }
		);
	}

	// Oracles repeat across markets that share a feed.
	const seen = new Set<string>();
	return entries.filter((e) => {
		const key = e.address.toBase58();
		if (seen.has(key)) {
			return false;
		}
		seen.add(key);
		return true;
	});
}

/** The env's configured market lookup table, unless one is passed explicitly. */
function resolveLut(opts: any, explicit?: string): PublicKey {
	if (explicit) {
		return new PublicKey(explicit);
	}
	const config = initialize({ env: opts.env });
	const tables = config.MARKET_LOOKUP_TABLES;
	if (!tables?.length) {
		throw new Error(
			`no MARKET_LOOKUP_TABLES configured for env "${opts.env}"; pass the address explicitly`
		);
	}
	if (tables.length > 1) {
		throw new Error(
			`env "${opts.env}" configures ${tables.length} lookup tables; pass the one to act on explicitly`
		);
	}
	return new PublicKey(tables[0]);
}

export function registerLut(parent: Command): void {
	const lut = parent
		.command('lut')
		.description(
			'Market address lookup table (used to fit market and oracle accounts into versioned transactions).'
		);

	withGlobalOptions(
		lut
			.command('show [address]')
			.description(
				"Read the market lookup table: capacity, authority, and which of the live markets' accounts it is missing. " +
					"Defaults to the env's configured table. Read-only."
			)
	).action(async (address: string | undefined, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const lutPk = resolveLut(opts, address);
			const res = await provider.connection.getAddressLookupTable(lutPk);
			if (!res.value) {
				throw new Error(`lookup table ${lutPk.toBase58()} does not exist`);
			}
			const present = new Set(
				res.value.state.addresses.map((a) => a.toBase58())
			);
			const wanted = await collectMarketAddresses(client);
			const missing = wanted.filter((e) => !present.has(e.address.toBase58()));

			console.log(`lookup table ${lutPk.toBase58()}`);
			console.log(
				`  addresses: ${res.value.state.addresses.length} of ${LUT_CAPACITY}`
			);
			console.log(
				`  authority: ${
					res.value.state.authority?.toBase58() ?? 'FROZEN (cannot be extended)'
				}`
			);
			console.log(`  market accounts required: ${wanted.length}`);
			if (missing.length === 0) {
				console.log('  missing: none, the table covers every live market');
				return;
			}
			console.log(`  missing: ${missing.length}`);
			for (const e of missing) {
				console.log(`    ${e.label.padEnd(24)} ${e.address.toBase58()}`);
			}
		} finally {
			await client.unsubscribe();
		}
	});

	withGlobalOptions(
		lut
			.command('extend [address]')
			.description(
				'Add every live market account the lookup table is missing. The market set is derived ' +
					"from State's market counts, so it cannot go stale when a market is added, and " +
					'addresses already present are skipped. The signer must be the table authority. ' +
					"Defaults to the env's configured table."
			)
			.option('--dry-run', 'print what would be added, send nothing', false)
	).action(async (address: string | undefined, _flags, cmd: Command) => {
		const opts = readGlobalOpts(cmd);
		const local = cmd.opts() as { dryRun: boolean };
		const provider = buildProvider(opts);
		const client = await buildAdminClient(opts);
		try {
			const lutPk = resolveLut(opts, address);
			const res = await provider.connection.getAddressLookupTable(lutPk);
			if (!res.value) {
				throw new Error(`lookup table ${lutPk.toBase58()} does not exist`);
			}
			const authority = res.value.state.authority;
			if (!authority) {
				throw new Error(
					`lookup table ${lutPk.toBase58()} is frozen and can never be extended`
				);
			}

			const present = new Set(
				res.value.state.addresses.map((a) => a.toBase58())
			);
			const missing = (await collectMarketAddresses(client)).filter(
				(e) => !present.has(e.address.toBase58())
			);
			if (missing.length === 0) {
				console.log(
					`lookup table ${lutPk.toBase58()} already covers every live market, nothing to add`
				);
				return;
			}
			const total = res.value.state.addresses.length + missing.length;
			if (total > LUT_CAPACITY) {
				throw new Error(
					`adding ${missing.length} would take the table to ${total}, over the ${LUT_CAPACITY} limit`
				);
			}

			for (const e of missing) {
				console.log(`  + ${e.label.padEnd(24)} ${e.address.toBase58()}`);
			}
			console.log(
				`${missing.length} to add, table goes ${
					res.value.state.addresses.length
				} -> ${total}; authority ${authority.toBase58()}`
			);

			// Chunked so each extend transaction stays under the size limit.
			const ixs = [];
			for (let i = 0; i < missing.length; i += EXTEND_CHUNK) {
				ixs.push(
					AddressLookupTableProgram.extendLookupTable({
						lookupTable: lutPk,
						authority,
						payer: authority,
						addresses: missing.slice(i, i + EXTEND_CHUNK).map((e) => e.address),
					})
				);
			}

			const label = `lut ${lutPk.toBase58()} extended with ${
				missing.length
			} market account(s)`;
			const multisigPda = opts.multisig
				? new PublicKey(opts.multisig)
				: undefined;
			if (local.dryRun) {
				await reportDryRun(provider, ixs, multisigPda);
				return;
			}
			const result = await sendOrPropose(
				provider,
				ixs,
				multisigPda,
				'velocity-admin lut extend'
			);
			reportDispatch(label, result);
		} finally {
			await client.unsubscribe();
		}
	});
}
