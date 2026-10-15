import crypto from 'crypto';
import * as StellarSDK from '@stellar/stellar-sdk';
import { horizonServer, networkPassphrase } from '../config/stellar.js';
import * as StellarService from '../services/stellar.js';

/**
 * Offline transaction queue.
 * Clients enqueue transactions while offline; the server processes them
 * in order when the client reconnects and calls /mobile/queue/flush.
 */
class OfflineQueue {
  constructor() {
    // userId -> ordered array of queued transactions
    this.queues = new Map();
  }

  enqueue(userId, transaction) {
    if (!this.queues.has(userId)) this.queues.set(userId, []);
    const item = {
      id: crypto.randomUUID(),
      userId,
      transaction,
      enqueuedAt: new Date(),
      status: 'pending',
    };
    this.queues.get(userId).push(item);
    return item;
  }

  getQueue(userId) {
    return this.queues.get(userId) || [];
  }

  /** Mark an item as processed (success or failed) */
  updateStatus(userId, itemId, status, result = null) {
    const queue = this.queues.get(userId) || [];
    const item = queue.find(i => i.id === itemId);
    if (!item) throw new Error('Queue item not found');
    item.status = status;
    item.result = result;
    item.processedAt = new Date();
    return item;
  }

  /** Remove completed/failed items */
  clearProcessed(userId) {
    const queue = this.queues.get(userId) || [];
    const pending = queue.filter(i => i.status === 'pending');
    this.queues.set(userId, pending);
    return { removed: queue.length - pending.length };
  }

  /**
   * Classify a queued transaction as a client-signed XDR or an unsigned /
   * server-signed payment intent.
   */
  classifyTransaction(tx) {
    if (!tx || typeof tx !== 'object') return { kind: 'unknown' };
    if (tx.signedXdr || (tx.xdr && tx.signed !== false && !tx.sourceSecret)) {
      return { kind: 'signed_xdr', xdr: tx.signedXdr || tx.xdr };
    }
    if (tx.sourceSecret && tx.destination && tx.amount != null) {
      return { kind: 'server_signed', ...tx };
    }
    if (tx.destination && tx.amount != null && (tx.sourcePublicKey || tx.publicKey)) {
      return {
        kind: 'unsigned_intent',
        sourcePublicKey: tx.sourcePublicKey || tx.publicKey,
        destination: tx.destination,
        amount: tx.amount,
        assetCode: tx.assetCode || 'XLM',
      };
    }
    return { kind: 'unknown' };
  }

  /**
   * Extract the source account public key and embedded sequence from a
   * queued transaction, when available.
   */
  inspectTransaction(tx) {
    const classified = this.classifyTransaction(tx);
    if (classified.kind === 'signed_xdr') {
      const transaction = StellarSDK.TransactionBuilder.fromXDR(
        classified.xdr,
        networkPassphrase,
      );
      return {
        kind: 'signed_xdr',
        sourcePublicKey: transaction.source,
        embeddedSequence: BigInt(transaction.sequence),
        xdr: classified.xdr,
        transaction,
        hasSignatures: (transaction.signatures?.length ?? 0) > 0,
      };
    }
    if (classified.kind === 'server_signed') {
      const kp = StellarSDK.Keypair.fromSecret(classified.sourceSecret);
      return {
        kind: 'server_signed',
        sourcePublicKey: kp.publicKey(),
        embeddedSequence: tx.sequence != null ? BigInt(tx.sequence) : null,
        ...classified,
      };
    }
    if (classified.kind === 'unsigned_intent') {
      return {
        kind: 'unsigned_intent',
        sourcePublicKey: classified.sourcePublicKey,
        embeddedSequence: tx.sequence != null ? BigInt(tx.sequence) : null,
        ...classified,
      };
    }
    return { kind: 'unknown' };
  }

  /**
   * Flush pending queue items with pre-flight sequence verification.
   * Detects sequence drift before Horizon submission and either re-sequences
   * server-signed intents or returns SEQUENCE_MISMATCH re-sign prompts for
   * client-signed XDRs that cannot be mutated without a new signature.
   *
   * @param {string} userId
   * @param {{ submitPayment?: Function, loadAccount?: Function }} [deps]
   * @returns {Promise<{ processed: number, results: object[], requiresResign: boolean, sequenceDrift?: object }>}
   */
  async flushQueue(userId, deps = {}) {
    const submitPayment = deps.submitPayment || StellarService.sendPayment.bind(StellarService);
    const loadAccount =
      deps.loadAccount || ((publicKey) => horizonServer.loadAccount(publicKey));

    const pending = this.getQueue(userId).filter(i => i.status === 'pending');
    const results = [];
    let requiresResign = false;
    let sequenceDrift = null;
    /** @type {Map<string, bigint>} live sequence cursor per account */
    const liveSequences = new Map();

    const getLiveSequence = async (publicKey) => {
      if (liveSequences.has(publicKey)) return liveSequences.get(publicKey);
      const account = await loadAccount(publicKey);
      // Horizon sequence is the last used sequence; next tx must use +1
      const nextSeq = BigInt(account.sequenceNumber()) + 1n;
      liveSequences.set(publicKey, nextSeq);
      return nextSeq;
    };

    for (const item of pending) {
      try {
        const inspected = this.inspectTransaction(item.transaction);

        if (inspected.kind === 'unknown') {
          throw new Error('Unrecognized queued transaction format');
        }

        const liveSeq = await getLiveSequence(inspected.sourcePublicKey);
        const driftDetected =
          inspected.embeddedSequence != null && inspected.embeddedSequence !== liveSeq;

        if (driftDetected || (inspected.kind === 'signed_xdr' && inspected.embeddedSequence !== liveSeq)) {
          if (inspected.kind === 'signed_xdr' && inspected.hasSignatures) {
            // Cannot mutate signed XDR without invalidating signatures
            const drift = {
              code: 'SEQUENCE_MISMATCH',
              account: inspected.sourcePublicKey,
              expectedSequence: liveSeq.toString(),
              embeddedSequence: inspected.embeddedSequence?.toString() ?? null,
              drift: inspected.embeddedSequence != null
                ? (liveSeq - inspected.embeddedSequence).toString()
                : null,
              queueItemId: item.id,
              resignPayload: {
                originalXdr: inspected.xdr,
                suggestedSequence: liveSeq.toString(),
                remainingQueueItemIds: pending
                  .filter(i => i.status === 'pending')
                  .slice(pending.indexOf(item))
                  .map(i => i.id),
              },
            };

            this.updateStatus(userId, item.id, 'SEQUENCE_MISMATCH', drift);
            results.push({
              id: item.id,
              status: 'SEQUENCE_MISMATCH',
              error: 'Sequence number drift detected; client re-signature required',
              sequenceDrift: drift,
            });
            requiresResign = true;
            sequenceDrift = drift;
            // Do not permanently halt — mark remaining pending items as needing re-sign
            // but leave them pending so the client can replace/re-sign and flush again.
            break;
          }

          // Server-signed / unsigned intents: rebuild with current live sequence
          // by letting sendPayment load the account fresh (or pass through).
        }

        if (inspected.kind === 'signed_xdr') {
          // Sequence matches — submit the signed envelope as-is
          const result = await horizonServer.submitTransaction(inspected.transaction);
          this.updateStatus(userId, item.id, 'success', result);
          results.push({ id: item.id, status: 'success', hash: result.hash });
          liveSequences.set(inspected.sourcePublicKey, liveSeq + 1n);
        } else if (inspected.kind === 'server_signed') {
          const result = await submitPayment(
            inspected.sourceSecret,
            inspected.destination,
            String(inspected.amount),
            inspected.assetCode || 'XLM',
          );
          this.updateStatus(userId, item.id, 'success', result);
          results.push({ id: item.id, status: 'success', hash: result.hash });
          liveSequences.set(inspected.sourcePublicKey, liveSeq + 1n);
        } else if (inspected.kind === 'unsigned_intent') {
          // Unsigned intents require client signing — surface as re-sign prompt
          const drift = {
            code: 'SEQUENCE_MISMATCH',
            account: inspected.sourcePublicKey,
            expectedSequence: liveSeq.toString(),
            embeddedSequence: inspected.embeddedSequence?.toString() ?? null,
            queueItemId: item.id,
            resignPayload: {
              intent: {
                sourcePublicKey: inspected.sourcePublicKey,
                destination: inspected.destination,
                amount: String(inspected.amount),
                assetCode: inspected.assetCode || 'XLM',
              },
              suggestedSequence: liveSeq.toString(),
            },
          };
          this.updateStatus(userId, item.id, 'SEQUENCE_MISMATCH', drift);
          results.push({
            id: item.id,
            status: 'SEQUENCE_MISMATCH',
            error: 'Unsigned intent requires client signature with current sequence',
            sequenceDrift: drift,
          });
          requiresResign = true;
          sequenceDrift = drift;
          break;
        }
      } catch (e) {
        this.updateStatus(userId, item.id, 'failed', { error: e.message });
        results.push({ id: item.id, status: 'failed', error: e.message });
      }
    }

    this.clearProcessed(userId);

    const response = {
      processed: results.length,
      results,
      requiresResign,
    };
    if (sequenceDrift) response.sequenceDrift = sequenceDrift;
    return response;
  }
}

export default new OfflineQueue();
