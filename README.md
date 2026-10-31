# tenma/discord

Discord bots in PHP on [Tenma](https://github.com/tenma-runtime/tenma). A native plugin holds the gateway, REST and voice
connections; PHP gets the events and answers through a small API. Audio runs in the plugin, so a radio stream costs
PHP nothing.

```php
<?php

require __DIR__ . '/vendor/autoload.php';

use Tenma\Discord\Bot;
use Tenma\Discord\Interactions\Command;

$bot = new Bot(getenv('DISCORD_TOKEN'));
$bot->command('ping', 'Answers with pong', fn (Command $command) => 'pong');
$bot->start();
```

## Install

Needs Tenma 0.2 on macOS (Apple silicon) or Linux (x64, arm64).

```bash
composer require tenma/discord
vendor/bin/tenma-discord install
DISCORD_TOKEN=... tenma bot.php
```

`tenma-discord install` downloads the plugin of the installed version for this machine, checks its SHA-256, saves it
as `vendor/tenma/discord/lib/tenma_discord.plugin` and adds it to `plugins` in the project's `tenma.json`. The name is
the same on every platform, so a committed `tenma.json` works on macOS and Linux. Run it again after
`composer update`.

In the [Discord developer portal](https://discord.com/developers/applications): create an application, copy the bot's
token, and invite the bot with the scopes `bot` and `applications.commands`. The bot needs no public URL: everything
arrives over the gateway.

## Commands, buttons and modals

A handler's return value is the reply: a string, or a `Message` with embeds and buttons. Commands sync with Discord on
start, only when they changed.

```php
use Tenma\Discord\{Button, ButtonStyle, Embed, Message, Modal, Option, TextInput};
use Tenma\Discord\Interactions\{Autocomplete, Click, Command, Submit};

$bot->command(
    'poll',
    'Starts a poll',
    fn (Command $command) => new Message(
        embeds: [new Embed(title: $command->string('question'))],
        buttons: [new Button('vote:yes', 'Yes'), new Button('vote:no', 'No', ButtonStyle::Secondary)],
    ),
    [Option::string('question', 'What to ask', required: true)],
);

// "vote" receives every button whose id starts with "vote:"
$bot->button('vote', fn (Click $click) => new Message("{$click->user->mention()} voted", ephemeral: true));
```

- `$command->defer()` when the work takes longer than 3 seconds; the return value then replaces the "thinking" message.
- `$click->update($message)` changes the message under the button. Fields left null stay as they are, `[]` clears them.
- `Option::string('city', 'City', autocomplete: fn (Autocomplete $typed) => [...])` suggests values while the user types.
- `$command->modal(new Modal('feedback', 'Feedback', [new TextInput('text', 'Your feedback', paragraph: true)]))`
  opens a form; `$bot->modal('feedback', fn (Submit $submit) => ...)` gets what was entered.
- A handler that throws tells the user "Something went wrong." and the exception is thrown on.
- Mentions in a message ping nobody, so a bot that echoes user text cannot reach `@everyone`. `mentions:` lets them
  through, as Discord's `allowed_mentions`: `new Message("{$user->mention()} won", mentions: ['users' => [$user->id]])`
  pings that user, `mentions: ['parse' => ['users', 'roles', 'everyone']]` everyone the text mentions.

## Events

```php
use Tenma\Discord\Events\{Dispatch, MemberJoined, MessageCreated};

$bot->on(MessageCreated::class, function (MessageCreated $message) {
    if ($message->content === '!hello') {
        $message->reply('Hello');
    }
});
```

`Ready`, `GuildCreated`, `MessageCreated`, `VoiceStateUpdated`, `MemberJoined`, `TrackStarted`, `TrackEnded`,
`TrackFailed`, and `Dispatch` for every event as Discord sent it. Message content and members need the privileged
intents `Intents::MessageContent` and `Intents::GuildMembers`, switched on in the developer portal and passed to
`new Bot($token, $intents)`.

## Voice

```php
$bot->voice($guildId)->join($channelId)->play('https://listen.moe/stream');
```

`play` replaces what plays, `queue` adds to the end; `skip`, `pause`, `resume`, `stop`, `mute`, `tracks` and `leave`.
Streams, MP3, Ogg and AAC play. Only http and https URLs on public addresses play, also after redirects:
loopback, private, link-local and similar addresses throw a `RuntimeException`, so a user's URL cannot reach the
bot host's own network. A queue holds 50 tracks at most. `$command->voiceChannel` is the channel the user sits in. Voice uses Discord's
end-to-end encryption.

## REST

`$bot->rest->request('PUT', "/guilds/$guild/members/$user/roles/$role")` reaches the whole
[Discord API](https://discord.com/developers/docs) with the bot's token; `$bot->rest->send($channelId, $message)` posts
a message.

## Workers

Under `tenma --workers N` every worker runs the script; the plugin keeps one gateway connection and gives each worker
the events of its share of the guilds, so one guild's events stay in order on one worker. State that all workers need
belongs in Postgres or Redis.

## Example

[`examples/radio.php`](examples/radio.php) is a radio bot with `/radio`, `/play`, `/skip`, `/queue`, a mute button,
station suggestions and a modal.

## Building the plugin

Needs Rust and CMake (for the statically linked Opus).

```bash
cd plugin && cargo build --release && mkdir -p ../lib
cp target/release/libtenma_discord.dylib ../lib/tenma_discord.plugin   # .so on Linux
cd .. && DISCORD_TOKEN=... tenma --plugin lib/tenma_discord.plugin examples/radio.php
```

## License

MIT or Apache-2.0, at your option.
