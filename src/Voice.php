<?php

namespace Tenma\Discord;

/**
 * The bot's voice connection in one guild. Audio runs in the plugin: playing a stream costs PHP nothing.
 *
 *     $bot->voice($guildId)->join($channelId)->play('https://listen.moe/stream');
 */
final class Voice
{
    public function __construct(
        private readonly Connection $connection,
        public readonly string $guildId,
    ) {}

    /** Joins (or moves to) a voice channel and waits until the connection stands. */
    public function join(string $channelId): self
    {
        $this->send('join', ['channel' => $channelId]);

        return $this;
    }

    /**
     * Plays a URL (a radio stream, an MP3, an Ogg file) at once, in place of what plays and what is queued.
     *
     * @throws \RuntimeException when the URL is no http or https URL on a public address
     */
    public function play(string $url): self
    {
        $this->send('play', ['url' => $url]);

        return $this;
    }

    /**
     * Plays a URL after the tracks already queued, or at once if nothing plays.
     *
     * @throws \RuntimeException when the URL is no http or https URL on a public address, or the queue
     *     holds 50 tracks already
     */
    public function queue(string $url): self
    {
        $this->send('queue', ['url' => $url]);

        return $this;
    }

    /** Ends the current track; the next one in the queue starts. */
    public function skip(): self
    {
        $this->send('skip', []);

        return $this;
    }

    /** Pauses the current track. A radio stream keeps arriving meanwhile: mute() suits it better. */
    public function pause(): self
    {
        $this->send('pause', []);

        return $this;
    }

    public function resume(): self
    {
        $this->send('resume', []);

        return $this;
    }

    /**
     * The URLs in the queue, the one playing first.
     *
     * @return list<string>
     */
    public function tracks(): array
    {
        $tracks = json_decode($this->send('tracks', []), true, flags: JSON_THROW_ON_ERROR);

        return is_array($tracks) ? array_values(array_filter($tracks, is_string(...))) : [];
    }

    /** Stops what plays and empties the queue. */
    public function stop(): self
    {
        $this->send('stop', []);

        return $this;
    }

    /** Mutes the bot (or unmutes it with false): the stream keeps running, Discord shows the bot muted. */
    public function mute(bool $muted = true): self
    {
        $this->send('mute', ['muted' => $muted]);

        return $this;
    }

    /** Leaves the voice channel, also one joined before a restart. */
    public function leave(): void
    {
        $this->send('leave', []);
    }

    /** @param array<string, string|bool> $input */
    private function send(string $function, array $input): string
    {
        $input['guild'] = $this->guildId;

        return $this->connection->await("voice.$function", json_encode($input, JSON_THROW_ON_ERROR));
    }
}
