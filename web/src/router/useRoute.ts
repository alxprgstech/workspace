import { useCallback, useEffect, useState } from 'react'
import { parseRoute, routeToHash, sameRoute, type Route } from './core.ts'

/** Read the route from the hash and navigate by changing it (history-friendly). */
export function useRoute(): [Route, (next: Route) => void] {
  const [route, setRoute] = useState<Route>(() => parseRoute(window.location.hash))

  useEffect(() => {
    const update = () => setRoute(parseRoute(window.location.hash))

    window.addEventListener('hashchange', update)
    window.addEventListener('popstate', update)

    return () => {
      window.removeEventListener('hashchange', update)
      window.removeEventListener('popstate', update)
    }
  }, [])

  const navigate = useCallback((next: Route) => {
    const target = routeToHash(next)

    if (sameRoute(next, parseRoute(window.location.hash))) {
      setRoute(next)

      return
    }

    window.location.hash = target
    setRoute(next)
  }, [])

  return [route, navigate]
}
