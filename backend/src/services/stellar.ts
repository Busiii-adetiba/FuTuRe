/**
 * Canonical Stellar service source of truth lives in stellar.js.
 * This shim preserves TypeScript import compatibility without maintaining
 * a second independent implementation.
 */
export * from './stellar.js';
