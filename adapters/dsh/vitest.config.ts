import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    testTimeout: 30_000,
    hookTimeout: 30_000,
    projects: [
      {
        test: {
          name: 'unit',
          // Pure contracts and content checks; no native addon needed, safe to
          // run with file parallelism.
          include: [
            'tests/contracts.spec.ts',
            'tests/content.spec.ts',
            'tests/engine-pool.spec.ts',
            'tests/policy.spec.ts',
          ],
        },
      },
      {
        test: {
          name: 'native',
          include: [
            'tests/assembly.spec.ts',
            'tests/composition.spec.ts',
            'tests/native.spec.ts',
            'tests/real-server.e2e.ts',
          ],
          // These specs stage one shared addon and publish it through
          // process.env at module scope (see tests/helpers/composition.ts) and
          // spawn real process trees; the serialized constraint used to live
          // only in --maxWorkers=1 script flags.
          fileParallelism: false,
        },
      },
    ],
  },
})
