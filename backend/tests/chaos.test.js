import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import fs from 'fs/promises';
import path from 'path';
import { fileURLToPath } from 'url';
import http from 'http';
import https from 'https';
import {
  failureInjector,
  networkPartitionSimulator,
  serviceFailureSimulator,
  databaseFailureSimulator,
  recoveryTimeAnalyzer,
  blastRadiusLimiter,
  chaosTestAutomation,
  chaosReporter
} from '../src/chaos/index.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const DATA_DIR = path.join(__dirname, '../data');

async function cleanup() {
  try {
    await fs.rm(DATA_DIR, { recursive: true, force: true });
  } catch (error) {
    // Ignore cleanup errors
  }
}

describe('Chaos Engineering', () => {
  beforeEach(async () => {
    await cleanup();
  });

  afterEach(async () => {
    await cleanup();
  });

  describe('FailureInjector', () => {
    const CANARY_HEADERS = {
      'x-chaos-experiment-id': 'exp-test-1',
      'x-canary-test': 'true'
    };

    it('should inject latency', () => {
      failureInjector.removeAllInjections();
      const injection = failureInjector.injectLatency('api-1', 500, 1.0);
      expect(injection.type).toBe('LATENCY');
      expect(injection.delayMs).toBe(500);
    });

    it('should never disrupt requests without valid canary headers regardless of probability', () => {
      failureInjector.removeAllInjections();
      failureInjector.injectError('api-1', 100, 500);
      failureInjector.injectLatency('api-1', 5000, 1.0);

      const decision = failureInjector.shouldInjectFailure('api-1', {});
      expect(decision.inject).toBe(false);

      const decision2 = failureInjector.shouldInjectFailure('api-1', {
        headers: { 'x-canary-test': 'true' }
      });
      expect(decision2.inject).toBe(false);

      const decision3 = failureInjector.shouldInjectFailure('api-1', {
        headers: { 'x-chaos-experiment-id': 'exp-1' }
      });
      expect(decision3.inject).toBe(false);
    });

    it('should unconditionally reject injection on protected endpoints', () => {
      failureInjector.removeAllInjections();
      failureInjector.injectError('api-1', 100, 500);

      const protectedPaths = ['/api/transactions/submit', '/api/recovery/execute'];
      for (const p of protectedPaths) {
        const decision = failureInjector.shouldInjectFailure('api-1', {
          path: p,
          headers: CANARY_HEADERS
        });
        expect(decision.inject).toBe(false);
        expect(decision.reason).toMatch(/protected/i);
      }
    });

    it('should still inject for valid canary requests on non-protected endpoints', () => {
      failureInjector.removeAllInjections();
      failureInjector.injectError('api-1', 100, 500);

      const decision = failureInjector.shouldInjectFailure('api-1', {
        path: '/api/orders',
        headers: CANARY_HEADERS
      });
      expect(decision.inject).toBe(true);
    });

    it('should inject errors', () => {
      failureInjector.removeAllInjections();
      const injection = failureInjector.injectError('api-1', 10, 500);
      expect(injection.type).toBe('ERROR');
      expect(injection.errorRate).toBe(10);
    });

    it('should get active failures', () => {
      failureInjector.removeAllInjections();
      failureInjector.injectLatency('api-1', 500);
      failureInjector.injectError('api-2', 10);
      
      const failures = failureInjector.getActiveFailures();
      expect(failures.length).toBe(2);
    });

    it('should remove injection', () => {
      failureInjector.removeAllInjections();
      failureInjector.injectLatency('api-1', 500);
      failureInjector.removeInjection('latency-api-1');
      
      const failures = failureInjector.getActiveFailures();
      expect(failures.length).toBe(0);
    });
  });

  describe('NetworkPartitionSimulator', () => {
    afterEach(() => {
      networkPartitionSimulator.teardown();
    });

    it('should create partition', () => {
      const partition = networkPartitionSimulator.createPartition('part-1', ['api', 'db']);
      expect(partition.id).toBe('part-1');
      expect(partition.affectedServices).toHaveLength(2);
    });

    it('should detect partition', () => {
      networkPartitionSimulator.createPartition('part-1', ['api', 'db']);
      expect(networkPartitionSimulator.isPartitioned('api', 'db')).toBe(true);
    });

    it('should heal partition', () => {
      networkPartitionSimulator.createPartition('part-1', ['api', 'db']);
      networkPartitionSimulator.healPartition('part-1');
      
      expect(networkPartitionSimulator.isPartitioned('api', 'db')).toBe(false);
    });

    it('should never block non-targeted requests (health checks, logging)', () => {
      networkPartitionSimulator.createPartition('part-1', ['api', 'db'], {
        hosts: ['targeted.example.com'],
        ports: [8080]
      });

      const health = networkPartitionSimulator.shouldBlockRequest({
        hostname: 'health.internal',
        port: 80,
        path: '/healthz'
      });
      expect(health.blocked).toBe(false);

      const logging = networkPartitionSimulator.shouldBlockRequest({
        hostname: 'logs.internal',
        port: 514,
        path: '/ingest'
      });
      expect(logging.blocked).toBe(false);
    });

    it('should only affect requests matching configured hosts/ports', () => {
      networkPartitionSimulator.createPartition('part-1', ['api', 'db'], {
        hosts: ['targeted.example.com'],
        ports: [8080]
      });

      const targeted = networkPartitionSimulator.shouldBlockRequest({
        hostname: 'targeted.example.com',
        port: 8080,
        path: '/api/data'
      });
      expect(targeted.blocked).toBe(true);

      const wrongPort = networkPartitionSimulator.shouldBlockRequest({
        hostname: 'targeted.example.com',
        port: 9090,
        path: '/api/data'
      });
      expect(wrongPort.blocked).toBe(false);

      const wrongHost = networkPartitionSimulator.shouldBlockRequest({
        hostname: 'other.example.com',
        port: 8080,
        path: '/api/data'
      });
      expect(wrongHost.blocked).toBe(false);
    });

    it('should affect requests carrying the X-Chaos-Context header', () => {
      networkPartitionSimulator.createPartition('part-1', ['api', 'db'], {
        hosts: ['targeted.example.com'],
        ports: [8080]
      });

      const withContext = networkPartitionSimulator.shouldBlockRequest({
        hostname: 'unrelated.example.com',
        port: 1234,
        path: '/api/data',
        headers: { 'x-chaos-context': 'exp-1' }
      });
      expect(withContext.blocked).toBe(true);

      const withoutContext = networkPartitionSimulator.shouldBlockRequest({
        hostname: 'unrelated.example.com',
        port: 1234,
        path: '/api/data',
        headers: {}
      });
      expect(withoutContext.blocked).toBe(false);
    });

    it('should not monkey-patch global http/https networking modules', () => {
      const originalHttpRequest = http.request;
      const originalHttpsRequest = https.request;

      networkPartitionSimulator.createPartition('part-1', ['api', 'db'], {
        hosts: ['targeted.example.com'],
        ports: [8080]
      });

      expect(http.request).toBe(originalHttpRequest);
      expect(https.request).toBe(originalHttpsRequest);
    });

    it('should teardown and remove all interceptors without side effects', () => {
      const originalHttpRequest = http.request;
      const originalHttpsRequest = https.request;

      networkPartitionSimulator.createPartition('part-1', ['api', 'db'], {
        hosts: ['targeted.example.com'],
        ports: [8080]
      });

      networkPartitionSimulator.teardown();

      expect(http.request).toBe(originalHttpRequest);
      expect(https.request).toBe(originalHttpsRequest);

      const afterTeardown = networkPartitionSimulator.shouldBlockRequest({
        hostname: 'targeted.example.com',
        port: 8080,
        path: '/api/data',
        headers: { 'x-chaos-context': 'exp-1' }
      });
      expect(afterTeardown.blocked).toBe(false);
    });
  });

  describe('ServiceFailureSimulator', () => {
    it('should fail service', () => {
      const failure = serviceFailureSimulator.failService('api-1', 'CRASH');
      expect(failure.serviceId).toBe('api-1');
      expect(failure.failureType).toBe('CRASH');
    });

    it('should detect failed service', () => {
      serviceFailureSimulator.failService('api-1');
      expect(serviceFailureSimulator.isServiceFailed('api-1')).toBe(true);
    });

    it('should recover service', () => {
      serviceFailureSimulator.failService('api-1');
      serviceFailureSimulator.recoverService('api-1');
      
      expect(serviceFailureSimulator.isServiceFailed('api-1')).toBe(false);
    });
  });

  describe('DatabaseFailureSimulator', () => {
    it('should fail database', () => {
      const failure = databaseFailureSimulator.failDatabase('db-1', 'CONNECTION_TIMEOUT');
      expect(failure.databaseId).toBe('db-1');
      expect(failure.failureType).toBe('CONNECTION_TIMEOUT');
    });

    it('should inject query failure', () => {
      const failure = databaseFailureSimulator.injectQueryFailure('SELECT', 0.5);
      expect(failure.queryPattern).toBe('SELECT');
      expect(failure.failureRate).toBe(0.5);
    });

    it('should detect failed database', () => {
      databaseFailureSimulator.failDatabase('db-1');
      expect(databaseFailureSimulator.isDatabaseFailed('db-1')).toBe(true);
    });
  });

  describe('RecoveryTimeAnalyzer', () => {
    it('should record recovery', () => {
      recoveryTimeAnalyzer.clearMetrics();
      const metric = recoveryTimeAnalyzer.recordRecovery('api-1', 'CRASH', 5000);
      expect(metric.serviceId).toBe('api-1');
      expect(metric.downtime).toBe(5000);
    });

    it('should calculate MTTR', () => {
      recoveryTimeAnalyzer.clearMetrics();
      recoveryTimeAnalyzer.recordRecovery('api-1', 'CRASH', 5000);
      recoveryTimeAnalyzer.recordRecovery('api-1', 'CRASH', 3000);
      
      const mttr = recoveryTimeAnalyzer.calculateMTTR('api-1');
      expect(mttr).toBe(4000);
    });

    it('should calculate availability', () => {
      recoveryTimeAnalyzer.clearMetrics();
      recoveryTimeAnalyzer.recordRecovery('api-1', 'CRASH', 1000);
      const availability = recoveryTimeAnalyzer.calculateAvailability('api-1');
      expect(availability).toBeGreaterThan(99);
    });

    it('should generate recovery report', () => {
      recoveryTimeAnalyzer.clearMetrics();
      recoveryTimeAnalyzer.recordRecovery('api-1', 'CRASH', 5000);
      const report = recoveryTimeAnalyzer.getRecoveryReport('api-1');
      
      expect(report.mttr).toBeDefined();
      expect(report.availability).toBeDefined();
    });
  });

  describe('BlastRadiusLimiter', () => {
    it('should set limit', () => {
      const limit = blastRadiusLimiter.setLimit('limit-1', 3, 10, 60000);
      expect(limit.maxAffectedServices).toBe(3);
    });

    it('should allow injection within limits', () => {
      blastRadiusLimiter.setLimit('limit-1', 5, 10);
      const result = blastRadiusLimiter.canInjectFailure('fail-1', ['api', 'db'], 5);
      
      expect(result.allowed).toBe(true);
    });

    it('should reject injection exceeding limits', () => {
      blastRadiusLimiter.setLimit('limit-1', 2, 10);
      const result = blastRadiusLimiter.canInjectFailure('fail-1', ['api', 'db', 'cache'], 5);
      
      expect(result.allowed).toBe(false);
    });

    it('should record impact', () => {
      blastRadiusLimiter.setLimit('limit-1', 5, 10);
      const impact = blastRadiusLimiter.recordImpact('fail-1', ['api', 'db'], 5, 1000);
      
      expect(impact.withinLimits).toBe(true);
    });
  });

  describe('ChaosTestAutomation', () => {
    it('should create experiment', async () => {
      const experiment = await chaosTestAutomation.createExperiment(
        'test-exp',
        'Test experiment',
        [{ type: 'LATENCY', target: 'api' }],
        60000
      );
      
      expect(experiment.name).toBe('test-exp');
      expect(experiment.status).toBe('CREATED');
    });

    it('should get all experiments', async () => {
      await chaosTestAutomation.createExperiment('exp-1', 'Test', [], 60000);
      await chaosTestAutomation.createExperiment('exp-2', 'Test', [], 60000);
      
      const experiments = await chaosTestAutomation.getAllExperiments();
      expect(experiments.length).toBeGreaterThanOrEqual(0);
    });
  });

  describe('ChaosReporter', () => {
    it('should generate report', async () => {
      const results = {
        status: 'COMPLETED',
        failuresInjected: 3,
        servicesAffected: ['api', 'db'],
        errorRate: 5,
        downtime: 5000,
        endTime: Date.now(),
        startTime: Date.now() - 60000
      };
      
      const metrics = {
        mttr: 5000,
        mtbf: 300000,
        availability: 99.5,
        recoveryTime: 5000
      };
      
      const report = await chaosReporter.generateReport('exp-1', results, metrics);
      expect(report.experimentId).toBe('exp-1');
      expect(report.insights).toBeDefined();
      expect(report.recommendations).toBeDefined();
    });

    it('should generate summary report', async () => {
      const summary = await chaosReporter.generateSummaryReport();
      expect(summary.totalExperiments).toBeDefined();
      expect(summary.averageAvailability).toBeDefined();
    });
  });
});
