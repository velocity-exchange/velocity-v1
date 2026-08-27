#!/usr/bin/env node
import { Command } from 'commander';
import { withGlobalOptions } from './lib/options';
import { registerAccountExtension } from './commands/accountExtension';
import { registerAuth } from './commands/auth';
import { registerCall } from './commands/call';
import { registerConfig } from './commands/config';
import { registerWhoami } from './commands/whoami';
import { registerExchange } from './commands/exchange';
import { registerFeatureFlags } from './commands/featureFlags';
import { registerFees } from './commands/fees';
import { registerInsuranceFund } from './commands/insuranceFund';
import { registerMultisig } from './commands/multisig';
import { registerPerpMarket } from './commands/perpMarket';
import { registerProgram } from './commands/program';
import { registerShow } from './commands/show';
import { registerSpotMarket } from './commands/spotMarket';
import { registerUser } from './commands/user';

const program = new Command();

program
	.name('velocity-admin')
	.description(
		[
			'Velocity v1 admin CLI.',
			'',
			'Each subcommand builds the appropriate instruction(s) and either signs them',
			'with --keypair (default) or, with --multisig <pda>, wraps them in a Squads V4',
			'vault transaction + proposal. Tier (cold / warm / hot) is enforced on-chain;',
			'whatever signs is what gets checked, so you choose the tier by choosing the',
			'key (or multisig) you pass — not by which command you run.',
			'',
			'Connection settings can come from a named profile instead of flags: set up',
			'profiles with `velocity-admin config init`, select one with -p/--profile',
			'(or VELOCITY_ADMIN_PROFILE, or the configured default). Explicit flags',
			'always override the profile. `whoami` reports which on-chain authorities',
			'the configured signer holds.',
		].join('\n')
	)
	.version('0.1.0')
	.showHelpAfterError()
	.showSuggestionAfterError();

// Global connection options are declared on every leaf (see options.ts), but
// also on the root so `velocity-admin -p <profile> <command...>` works;
// users reasonably put the profile first. `optsWithGlobals` merges both.
withGlobalOptions(program);

registerConfig(program);
registerWhoami(program);
registerShow(program);
registerAuth(program);
registerPerpMarket(program);
registerSpotMarket(program);
registerExchange(program);
registerFeatureFlags(program);
registerFees(program);
registerMultisig(program);
registerUser(program);
registerInsuranceFund(program);
registerProgram(program);
registerAccountExtension(program);
registerCall(program);

program.parseAsync(process.argv).catch((err) => {
	console.error(err instanceof Error ? err.message : err);
	process.exit(1);
});
