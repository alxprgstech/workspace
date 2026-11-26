// TLS reverse proxy for the isolated update lab.
//
// The production UI is served over HTTPS behind a reverse proxy, and the server
// validates the Origin header against the SSO redirect URI. The lab therefore
// exposes the same HTTPS shape locally with a self-signed certificate; Chrome is
// started with --ignore-certificate-errors for the test profile only.

import http from 'node:http'
import https from 'node:https'
import { readFileSync } from 'node:fs'

const listenPort = Number(process.env.LAB_TLS_PORT ?? 8443)
const upstreamPort = Number(process.env.LAB_UPSTREAM_PORT ?? 8799)
const key = readFileSync(process.env.LAB_TLS_KEY ?? 'target/lab-tls/key.pem')
const cert = readFileSync(process.env.LAB_TLS_CERT ?? 'target/lab-tls/cert.pem')

const server = https.createServer({ key, cert }, (request, response) => {
  const headers = { ...request.headers, host: `127.0.0.1:${upstreamPort}` }

  const upstream = http.request(
    {
      host: '127.0.0.1',
      port: upstreamPort,
      path: request.url,
      method: request.method,
      headers,
    },
    reply => {
      response.writeHead(reply.statusCode ?? 502, reply.headers)
      reply.pipe(response)
    },
  )

  upstream.on('error', error => {
    if (!response.headersSent) {
      response.writeHead(502, { 'content-type': 'application/json' })
    }
    response.end(JSON.stringify({ error: 'lab_proxy_unavailable', message: error.message }))
  })

  request.pipe(upstream)
})

server.listen(listenPort, '127.0.0.1', () => {
  console.log(`lab TLS proxy https://localhost:${listenPort} -> http://127.0.0.1:${upstreamPort}`)
})
