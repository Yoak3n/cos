/**
 * @cos/bundles/real — the "real DeepSeek provider" bundle, aggregate-resolved.
 * @module @cos/bundles/real
 */

import type { Bundle } from '@cos/boot'

export const bundles: Array<string | Bundle> = ['@cos/bundle-base', '@cos/bundle-real']

export default bundles