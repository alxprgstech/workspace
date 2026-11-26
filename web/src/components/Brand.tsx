/**
 * Official ALXPRGS artwork, bundled locally from the SSO repository.
 * Source and license: web/public/brand/README.md, web/public/licenses/.
 */
export function Brand({ variant = 'navigation' }: { variant?: 'auth' | 'navigation' }) {
  const full = variant === 'auth'

  return (
    <span className={`brand-identity brand-${variant}`} data-testid="brand-identity">
      {full && (
        <img
          className="brand-art-wide"
          src="/brand/logo-adaptive.svg"
          alt="ALXPRGS"
          width={112}
          height={84}
          decoding="async"
        />
      )}
      <img
        className="brand-mark"
        src="/brand/logo-mark.svg"
        alt={full ? '' : 'ALXPRGS'}
        aria-hidden={full || undefined}
        width={full ? 44 : 32}
        height={full ? 44 : 32}
        decoding="async"
      />
      <span className="brand-wordmark">
        ALXPRGS <span>Workspace</span>
      </span>
    </span>
  )
}
