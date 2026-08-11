import { describe, expect, it } from 'vitest';
import { agenticProviderAvailable } from './test_providers_lib';

describe('agentic provider discovery', () => {
  it.each([
    [false, false, false],
    [true, false, false],
    [false, true, false],
    [true, true, true],
  ])(
    'requires both an executable and usable credentials',
    (commandAvailable, credentialAvailable, expected) => {
      expect(agenticProviderAvailable(commandAvailable, credentialAvailable)).toBe(expected);
    }
  );
});
