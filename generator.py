import markovify
import aiofiles
import re

with open("model1.json", "r") as model_file:
    model_json = model_file.read()
    model = markovify.Text.from_json(model_json)
    model.compile()

async def process_sample(message):
    # Regular expression to identify URLs
    url_pattern = re.compile(r"(https?://\S+)")
    parts = url_pattern.split(message) # Split the message into URL and non-URL parts
    # Apply lowercasing only to non-URL parts
    parts = [part.lower() if not url_pattern.match(part) else part for part in parts]
    return ''.join(parts)

async def generate():
    return model.make_short_sentence(max_chars=500, test_output=False)

async def archive(message_list):
    global model
    # Process each message before updating the model
    samples = [await process_sample(message) for message in message_list]

    new_model = markovify.Text(input_text=samples, state_size=1, retain_original=False)
    model = markovify.combine([model, new_model])

    model_json = model.to_json()
    async with aiofiles.open("model1.json", "w") as model_file:
        await model_file.write(model_json)

    model.compile()
