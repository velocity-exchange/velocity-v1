import { assert } from 'chai';
import {
	isOracleValidForMarginCalc,
	isOracleValidForTriggerOrder,
	OracleValidity,
} from '../../src';

// Each row is `is_oracle_valid_for_action(validity, Some(VelocityAction::TriggerOrder))`
// in programs/velocity/src/math/oracle.rs.
const TRIGGER_ORDER_EXPECTED: [OracleValidity, boolean][] = [
	[OracleValidity.NonPositive, false],
	[OracleValidity.TooVolatile, false],
	[OracleValidity.TooUncertain, false],
	[OracleValidity.StaleForMargin, false],
	[OracleValidity.InsufficientDataPoints, true],
	[OracleValidity.StaleForAMMLowRisk, true],
	[OracleValidity.isStaleForAmmImmediate, true],
	[OracleValidity.Valid, true],
];

describe('isOracleValidForTriggerOrder', () => {
	for (const [validity, expected] of TRIGGER_ORDER_EXPECTED) {
		it(`${OracleValidity[validity]} -> ${expected}`, () => {
			assert.equal(isOracleValidForTriggerOrder(validity), expected);
		});
	}

	it('admits the same set as MarginCalc', () => {
		for (const [validity] of TRIGGER_ORDER_EXPECTED) {
			assert.equal(
				isOracleValidForTriggerOrder(validity),
				isOracleValidForMarginCalc(validity)
			);
		}
	});
});
