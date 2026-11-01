import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import Translate, {translate} from '@docusaurus/Translate';
import styles from './styles.module.css';

type Feature = {title: string; description: string; to: string};

function features(): Feature[] {
  return [
    {
      title: translate({id: 'home.native.vertex.title', message: 'Native Vertex AI lane'}),
      description: translate({
        id: 'home.native.vertex.description',
        message:
          'Context caching, gs:// media URIs, strict responseSchema decoding and thinking config go straight to Vertex AI streamGenerateContent. Nothing is flattened through an OpenAI-shaped adapter.',
      }),
      to: '/docs/guides/native-vertex/',
    },
    {
      title: translate({id: 'home.native.tools.title', message: 'Native tool calling'}),
      description: translate({
        id: 'home.native.tools.description',
        message:
          'Tools work on the standard and native Vertex lanes, and on the native lane tool_choice maps to a Vertex toolConfig mode instead of being dropped.',
      }),
      to: '/docs/guides/streaming-and-tools/',
    },
    {
      title: translate({id: 'home.native.jevlane.title', message: 'Jev lane'}),
      description: translate({
        id: 'home.native.jevlane.description',
        message:
          'Ask TypeSafe System One typed questions and get structured decisions back, or judge candidates and extract only the survivors in one call.',
      }),
      to: '/docs/guides/jev-lane/',
    },
    {
      title: translate({id: 'home.native.jevrouter.title', message: 'Jev router'}),
      description: translate({
        id: 'home.native.jevrouter.description',
        message:
          'Jev rates each request’s difficulty and serves it from the matching model tier and reasoning effort. Effort becomes each provider’s own control, such as a Vertex thinking budget, and every response reports the decision in x-synapse-* headers.',
      }),
      to: '/docs/guides/jev-router/',
    },
    {
      title: translate({id: 'home.native.jevharness.title', message: 'Jev 2D harness router'}),
      description: translate({
        id: 'home.native.jevharness.description',
        message:
          'Route Universal Harness Protocol (POST /v1/responses) turns across a 2D (harness × model) grid—picking both the agent runtime (pi, claude-code, codex, hermes) and the difficulty tier in one Jev call, while pinning the harness base across session continuations.',
      }),
      to: '/docs/guides/jev-harness-router/',
    },
    {
      title: translate({id: 'home.native.guardrails.title', message: 'Input, output & Jev guardrails'}),
      description: translate({
        id: 'home.native.guardrails.description',
        message:
          'Combine fast in-process scanners, in-place [REDACTED:...] masking, TypeSafe Jev semantic noul classifiers and streaming output holdback scanning across chat, passthrough, embeddings and Jev lanes.',
      }),
      to: '/docs/configuration/guardrails-policy/',
    },
    {
      title: translate({id: 'home.native.passthrough.title', message: 'Native Gemini & Jev passthrough'}),
      description: translate({
        id: 'home.native.passthrough.description',
        message:
          'Point @google/genai SDKs or TypeSafe System One clients directly at the gateway. Generation calls walk Vertex fallback legs automatically while keeping guardrails and per-tenant ledger metering.',
      }),
      to: '/docs/reference/http-api/',
    },
    {
      title: translate({id: 'home.native.streaming.title', message: 'Real streaming, full fallback'}),
      description: translate({
        id: 'home.native.streaming.description',
        message:
          'On the standard and native Vertex lanes, Synapse always streams from upstream. Streaming clients get token-by-token SSE; buffered standard-lane clients keep the whole fallback chain, even after a mid-stream failure.',
      }),
      to: '/docs/guides/fallback-chains/',
    },
    {
      title: translate({id: 'home.native.embed.title', message: 'Embeddable'}),
      description: translate({
        id: 'home.native.embed.description',
        message:
          'Run one small Rust binary, or drop the library crate into your service and call Gateway::chat() in-process.',
      }),
      to: '/docs/guides/embedding-as-library/',
    },
  ];
}

export default function NativeFeatures(): ReactNode {
  return (
    <section className="sy-section">
      <div className="container">
        <p className="sy-eyebrow">
          <Translate id="home.native.eyebrow">Native first</Translate>
        </p>
        <h2 className="sy-section-title">
          <Translate id="home.native.title">Keep the capabilities other gateways throw away</Translate>
        </h2>
        <p className="sy-section-lead">
          <Translate id="home.native.lead">
            Most gateways reach every provider through one generic adapter. Synapse keeps dedicated native lanes, so provider-specific features survive the trip.
          </Translate>
        </p>
        <div className={styles.grid}>
          {features().map((feature) => (
            <Link key={feature.to} className="sy-card" to={feature.to}>
              <h3>{feature.title}</h3>
              <p>{feature.description}</p>
            </Link>
          ))}
        </div>
      </div>
    </section>
  );
}
