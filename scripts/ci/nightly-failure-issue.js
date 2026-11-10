// File a scheduled-job failure as ONE open issue per title (#4723).
//
// Called from actions/github-script in .github/workflows/scheduled-tests.yml:
//
//   const notify = require('./scripts/ci/nightly-failure-issue.js');
//   await notify({ github, context, title: '🚨 Nightly E2E Tests Failed',
//                  summary: 'The nightly E2E test run has failed.' });
//
// The notifiers used to call issues.create on every red night, so one broken
// suite produced a new copy of the same issue each day (16 "Nightly E2E
// Tests Failed" issues so far, #360 .. #4531). Now: if an open issue with
// exactly this title and the `automated` label exists, the run is added
// to it as a comment; otherwise a new issue is opened. Closing the issue once
// the job is green again means the next failure opens a fresh one.
module.exports = async ({ github, context, title, summary }) => {
  const { owner, repo } = context.repo;
  const runUrl = `${context.serverUrl}/${owner}/${repo}/actions/runs/${context.runId}`;
  const details = [
    `**Workflow:** ${context.workflow}`,
    `**Run:** ${context.runId}`,
    `**SHA:** ${context.sha}`,
    '',
    `[View the failed run](${runUrl})`,
  ].join('\n');

  const open = await github.paginate(github.rest.issues.listForRepo, {
    owner,
    repo,
    state: 'open',
    labels: 'automated',
    per_page: 100,
  });
  const existing = open
    .filter((i) => !i.pull_request && i.title === title)
    .sort((a, b) => a.number - b.number)[0];

  if (existing) {
    await github.rest.issues.createComment({
      owner,
      repo,
      issue_number: existing.number,
      body: `${summary} (again)\n\n${details}`,
    });
    console.log(`Commented on #${existing.number} (${title})`);
    return existing.number;
  }

  const { data: created } = await github.rest.issues.create({
    owner,
    repo,
    title,
    body: `${summary}\n\n${details}\n`,
    labels: ['type:bug', 'automated'],
  });
  console.log(`Opened #${created.number} (${title})`);
  return created.number;
};
