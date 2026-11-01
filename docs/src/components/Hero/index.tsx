import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import Translate from '@docusaurus/Translate';
import CodeBlock from '@theme/CodeBlock';
import styles from './styles.module.css';

const QUICKSTART = `docker run --rm -p 8080:8080 -p 9090:9090 \\
  -e VERTEX_PROJECT_ID=my-gcp-project \\
  -e OPENAI_API_KEY=sk-... \\
  -e GOOGLE_APPLICATION_CREDENTIALS=/secrets/sa.json \\
  -v "$(pwd)/sa.json:/secrets/sa.json:ro" \\
  -v "$(pwd)/config:/app/config" \\
  sustentabilitas/synapse-gateway

# In a second terminal:
curl -s http://localhost:8080/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -d '{
    "model": "gemini-flash",
    "messages": [{"role": "user", "content": "Describe this video."}],
    "vertex": {
      "media_uris": ["gs://cloud-samples-data/video/animals.mp4"]
    }
  }'`;

export default function Hero(): ReactNode {
  return (
    <header className={styles.hero}>
      <div className={`container ${styles.inner}`}>
        <div>
          <h1 className={styles.title}>
            <Translate id="home.hero.title.lead">The LLM gateway that keeps</Translate>{' '}
            <span className={styles.accent}>
              <Translate id="home.hero.title.accent">native power.</Translate>
            </span>
          </h1>
          <p className={styles.subtitle}>
            <Translate id="home.hero.subtitle">
              One Rust binary. OpenAI, Gemini and Universal Harness Protocol (UHP) on the outside; native Vertex AI, 1D/2D Jev routing and input/output guardrails on the inside. No lowest-common-denominator adapters.
            </Translate>
          </p>
          <div className={styles.buttons}>
            <Link className="button button--primary button--lg" to="/docs/get-started/quickstart/">
              <Translate id="home.hero.cta.start">Get started</Translate>
            </Link>
            <Link className="button button--secondary button--outline button--lg" href="https://github.com/sustentabilitas/synapse-gateway">
              <Translate id="home.hero.cta.github">Star on GitHub</Translate>
            </Link>
          </div>
        </div>
        <div className={styles.terminal}>
          <div className={styles.terminalBar} aria-hidden="true">
            <span className={styles.dot} />
            <span className={styles.dot} />
            <span className={styles.dot} />
          </div>
          <CodeBlock language="bash">{QUICKSTART}</CodeBlock>
        </div>
      </div>
    </header>
  );
}
