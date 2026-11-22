{{#if aborted}}
{{#if resumable}}
{{agentId}} was stopped but is still resumable — {{#if ircEnabled}}message it via `write agent://{{agentId}}` to resume; {{/if}}{{#if transcriptAvailable}}transcript at history://{{agentId}}{{else}}transcript unavailable{{/if}}
{{else}}
{{agentId}} was aborted — {{#if transcriptAvailable}}transcript at history://{{agentId}}{{else}}transcript unavailable{{/if}}
{{/if}}
{{else}}
{{#if isolated}}
{{agentId}} ran isolated; its workspace is retained, so it stays resumable — {{#if ircEnabled}}message it via `write agent://{{agentId}}` to continue there; {{/if}}transcript at history://{{agentId}}{{#if deferredIntegration}}
Its changes are held for explicit integration: write `/integrate` to `worktree://{{agentId}}` to apply the latest workspace changes to the parent checkout.{{/if}}
{{else}}
{{agentId}} is now idle — {{#if ircEnabled}}message it via `write agent://{{agentId}}` to follow up; {{/if}}transcript at history://{{agentId}}
{{/if}}
{{/if}}
