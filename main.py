import discord
import random
import generator
import asyncio
from datetime import datetime, timedelta
from collections import defaultdict

TOKEN = "YOUR_BOT_TOKEN"

intents = discord.Intents.default()
intents.message_content = True

class BotClient(discord.Client):
    def __init__(self, intents):
        super().__init__(intents=intents)
        self.message_count = random.randint(1, 20)
        self.new_world = None
        self.message_cache = defaultdict(list)

    async def on_ready(self):
        print(f'Logged on as {self.user}')
        self.new_world = await self.fetch_guild(848178224654581771) # Replace with your Guild ID
        await asyncio.create_task(self.message_archiver())

    async def on_message(self, message):
        # Only new world authorized messages
        if message.guild != self.new_world:
            return

        self.message_count -= 1

        # Mention logic
        if self.user.mentioned_in(message):
            async with message.channel.typing():
                response = await generator.generate()
                await message.reply(response, mention_author=False)

        # Decrease the count of messages even if it's the bot's own message
        # Only reset the message count if the bot isn't the author
        elif self.message_count < 1 and message.author != self.user:
            async with message.channel.typing():
                gen_message = await generator.generate()
                await message.channel.send(gen_message)
            self.message_count = random.randint(1, 20)

        # Only cache messages from users
        if message.author == self.user:
            return

        # Privacy extension (to reduce disk I/O actually)
        # 80-minute window to edit or delete your message before it's archived
        self.message_cache[message.channel.id].append({
            "message_id": message.id,
            "expiry_time": datetime.now() + timedelta(minutes=80)
        })

        # print(self.message_cache)

    async def message_archiver(self):
        while True:
            await asyncio.sleep(4800)
            message_list = []
            dead_channels = []

            # Skip if the cache is empty
            if not self.message_cache:
                continue

            for channel_id in self.message_cache:
                expired_messages = []

                messages = self.message_cache[channel_id]
                channel = self.get_channel(channel_id)

                for metadata in messages:
                    if metadata["expiry_time"] < datetime.now():
                        expired_messages.append(metadata)
                        try:
                            message = await channel.fetch_message(metadata["message_id"])
                            message_list.append(message.content)
                        except discord.NotFound:
                            pass
                        expired_messages.append(metadata)

                # Remove expired messages from the cache
                self.message_cache[channel_id] = [m for m in messages if m not in expired_messages]

                # If the channel is empty, mark it for deletion
                if not self.message_cache[channel_id]:
                    dead_channels.append(channel_id)
                    continue

            # Remove empty channels from the cache
            for channel_id in dead_channels:
                if not self.message_cache[channel_id]:
                    del self.message_cache[channel_id]

            print(self.message_cache)

            # Create a new task to archive the messages
            if message_list:
                await asyncio.create_task(generator.archive(message_list))

client = BotClient(intents=intents)
client.run(TOKEN)
