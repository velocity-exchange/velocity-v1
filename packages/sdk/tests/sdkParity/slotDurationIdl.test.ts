import { assert } from 'chai';
import velocityIDL from '../../src/idl/velocity.json';

type IdlField = { name: string; type: unknown };
type IdlStruct = {
	name: string;
	type: { kind: 'struct'; fields: IdlField[] };
};

function fieldType(structName: string, fieldName: string): unknown {
	const struct = (velocityIDL as { types: IdlStruct[] }).types.find(
		(type) => type.name === structName
	);
	if (!struct) {
		throw new Error(`IDL struct '${structName}' not found`);
	}
	const field = struct.type.fields.find(
		(candidate) => candidate.name === fieldName
	);
	if (!field) {
		throw new Error(`IDL field '${structName}.${fieldName}' not found`);
	}
	return field.type;
}

describe('stored slot-duration IDL compatibility', () => {
	it('keeps transparent Rust wrappers encoded as their legacy primitives', () => {
		assert.equal(fieldType('State', 'min_perp_auction_duration'), 'u8');
		assert.equal(fieldType('State', 'liquidation_duration'), 'u8');
		assert.equal(
			fieldType('ValidityGuardRails', 'slots_before_stale_for_amm'),
			'i64'
		);
		assert.equal(
			fieldType('ValidityGuardRails', 'slots_before_stale_for_margin'),
			'i64'
		);
		assert.equal(fieldType('Constituent', 'oracle_staleness_threshold'), 'u64');
	});
});
