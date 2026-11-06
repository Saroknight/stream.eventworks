// Command line helpers.
//
//   node server/cli.js admin-link [https://your.domain]
//
// Prints a one-time sign-in link for the admin account (valid 24 hours). Use it the first time,
// or any time you're locked out. On the server:
//   sudo streamroom-admin-link
import { ADMIN_LINK_HOURS, PUBLIC_URL } from './config.js';
import { adminLinkTicket } from './auth.js';
import { adminUser } from './store.js';

const args = process.argv.slice(2);
const cmd = args[0];
const base = args.slice(1).find((a) => !a.startsWith('--'));

if (cmd === 'admin-link') {
  // --if-no-admin: only print a link when no admin account exists yet (used by the installer).
  if (args.includes('--if-no-admin') && adminUser()) process.exit(1);
  const url = `${(base || PUBLIC_URL).replace(/\/+$/, '')}/join/${adminLinkTicket()}`;
  console.log(`\nAdmin sign-in link (works once, for ${ADMIN_LINK_HOURS} hours):\n\n  ${url}\n`);
} else {
  console.log('Usage: node server/cli.js admin-link [https://your.domain]');
  process.exit(1);
}
process.exit(0);
