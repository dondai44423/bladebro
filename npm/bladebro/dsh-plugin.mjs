import { resolveBinary } from './binary.cjs';

export const name = 'bladebro';
export const inject = ['tools'];

// DSH owns discovery, schemas, images, cancellation, reconnection and disposal.
// Import its bridge from the running host, so no private SDK copy can drift.
export async function apply(ctx, config = {}) {
  if (!config || typeof config !== 'object' || Array.isArray(config)) {
    throw new Error('bladebro: plugin config must be an object');
  }
  const { binaryPath, ...options } = config;
  for (const key of Object.keys(options)) {
    if (!['env', 'cwd', 'toolCallTimeoutMs', 'reconnect'].includes(key)) {
      throw new Error(`bladebro: unknown plugin option ${JSON.stringify(key)}`);
    }
  }
  if (options.toolCallTimeoutMs !== undefined &&
      (!Number.isSafeInteger(options.toolCallTimeoutMs) || options.toolCallTimeoutMs < 1000 || options.toolCallTimeoutMs > 2147483647)) {
    throw new Error('bladebro: toolCallTimeoutMs must be an integer from 1000 through 2147483647');
  }
  const command = resolveBinary(binaryPath);
  const bridge = await import('@deepseek-ai/dsh-mcp-client');
  await ctx.plugin(bridge, {
    toolCallTimeoutMs: 180000,
    ...options,
    transport: 'stdio',
    serverName: 'bladebro',
    command,
    args: ['mcp'],
    failOnStartupError: true,
  });
}
