export const FEATURE_FLAGS = {
	// TODO : Remove this once we're confident that NEW_ORACLE_DATA_IN_L2 works .. delete corresponding code
	OLD_ORACLE_PRICE_IN_L2: true,
	NEW_ORACLE_DATA_IN_L2: true,

	DISABLE_RATE_LIMIT: process.env.DISABLE_RATE_LIMIT
		? process.env.DISABLE_RATE_LIMIT.toLowerCase() === 'true'
		: false,
};

export default FEATURE_FLAGS;
