/**
 * Every instruction the SDK builds must exist in the IDL.
 *
 * Anchor types `program.instruction`, so a call to a removed instruction is
 * normally a compile error. A `program.instruction as any` cast defeats that,
 * and the SDK uses the cast wherever the generated types are awkward. The call
 * then compiles, ships, and fails at run time with an unknown instruction.
 *
 * This test reads the call sites out of the source and checks each name against
 * the IDL, which is the one place the program's real surface is written down.
 */
import { assert } from 'chai';
import * as fs from 'fs';
import * as path from 'path';
import velocityIdl from '../../src/idl/velocity.json';

const SRC = path.join(__dirname, '..', '..', 'src');

/** `placeAndTakePerpOrderV1` -> `place_and_take_perp_order_v1`. */
function toSnakeCase(name: string): string {
	return name
		.replace(/(?<!^)(?=[A-Z])/g, '_')
		.toLowerCase()
		.replace(/_(\d+)/g, '$1');
}

function tsFilesUnder(dir: string): string[] {
	return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
		const full = path.join(dir, entry.name);
		if (entry.isDirectory()) {
			return tsFilesUnder(full);
		}
		return entry.name.endsWith('.ts') ? [full] : [];
	});
}

/**
 * The instruction each `program.instruction as any` call names, with the file
 * and line it sits on. The cast and the method can be split across lines, so
 * the source is matched with whitespace collapsed and offsets mapped back.
 */
function castCallSites(): Array<{ file: string; line: number; name: string }> {
	const sites: Array<{ file: string; line: number; name: string }> = [];
	for (const file of tsFilesUnder(SRC)) {
		if (file.includes(`${path.sep}idl${path.sep}`)) {
			continue;
		}
		const source = fs.readFileSync(file, 'utf8');
		const pattern = /instruction\s+as\s+any\s*\)\s*\.\s*([A-Za-z0-9_]+)/g;
		let match: RegExpExecArray | null;
		while ((match = pattern.exec(source)) !== null) {
			sites.push({
				file: path.relative(SRC, file),
				line: source.slice(0, match.index).split('\n').length,
				name: match[1],
			});
		}
	}
	return sites;
}

/**
 * Call sites that already named a missing instruction before this test existed.
 * Each one is a real bug: the call compiles and fails on chain. They are listed
 * rather than fixed here because none of them is a trading path, and fixing an
 * admin endpoint belongs with whatever change owns it.
 *
 * The list may shrink. It must never grow: a new entry means a call site was
 * broken and waved through.
 */
const KNOWN_BROKEN = new Set([
	'resetAmmCache',
	'updatePerpMarketTargetBaseAssetAmountPerLp',
	'updateWhitelistMint',
	'updateMaxSlippageRatio',
	'updateFeatureBitFlagsBuilderReferral',
	'updateUserAdvancedLp',
	'updateUserOpenOrdersCount',
]);

describe('instruction names', () => {
	const known = new Set(velocityIdl.instructions.map((ix: any) => ix.name));

	it('finds the untyped call sites it is meant to guard', () => {
		// A refactor that changes how the cast is written would otherwise make
		// this suite pass by checking nothing.
		assert.isAbove(
			castCallSites().length,
			0,
			'no `program.instruction as any` call sites found; the pattern this test scans for has changed'
		);
	});

	it('names an instruction the program has', () => {
		const missing = castCallSites().filter(
			(site) =>
				!known.has(toSnakeCase(site.name)) && !KNOWN_BROKEN.has(site.name)
		);
		assert.deepStrictEqual(
			missing.map((site) => `${site.file}:${site.line} ${site.name}`),
			[],
			'these calls name an instruction that is not in the IDL, so they fail at run time'
		);
	});

	it('does not carry a stale exemption', () => {
		const stillBroken = new Set(
			castCallSites()
				.filter((site) => !known.has(toSnakeCase(site.name)))
				.map((site) => site.name)
		);
		const fixed = [...KNOWN_BROKEN].filter((name) => !stillBroken.has(name));
		assert.deepStrictEqual(
			fixed,
			[],
			'these are no longer broken; drop them from KNOWN_BROKEN'
		);
	});
});
